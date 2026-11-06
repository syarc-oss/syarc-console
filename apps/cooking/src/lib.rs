//! Cooking app: recipe search, menu calendar, shopping lists.
//! Ported from the Python (fastmain) implementation. Recipe/ingredient/menu
//! ids keep their UUID strings; user ids are console INTEGERs. The supply
//! app is always installed in the console, so its integration is always on.

use axum::extract::{Path, Query, State};
use axum::response::{Html, IntoResponse, Redirect, Response};
use axum::routing::{get, post};
use axum::Router;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState, User};

pub const MEAL_TYPES: [&str; 4] = ["breakfast", "lunch", "dinner", "snack"];

pub struct CookingPlugin;

impl CookingPlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("cooking", "Cooking", "/cooking").icon("bi-cup-hot");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for CookingPlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    /// Grant cooking to existing active human accounts (idempotent) so the
    /// ported data stays reachable; new registrations do not get it by default.
    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id) \
             SELECT id, 'cooking' FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%'",
        )
        .execute(db)
        .await?;
        Ok(())
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {        Router::new()
            .route("/cooking", get(landing))
            .route("/cooking/", get(landing))
            .route("/cooking/search", get(search_page))
            .route("/cooking/recipes/new", get(new_recipe_page).post(create_recipe))
            .route("/cooking/recipes/{recipe_id}", get(recipe_detail))
            .route("/cooking/recipes/{recipe_id}/edit", get(edit_recipe_page).post(update_recipe))
            .route("/cooking/recipes/{recipe_id}/delete", post(delete_recipe))
            .route("/cooking/recipes/{recipe_id}/add-to-menu", post(add_to_menu))
            .route("/cooking/menu", get(menu_calendar))
            .route("/cooking/menu/{entry_id}/delete", post(delete_menu_entry))
            .route("/cooking/menu/{entry_id}/cook", post(assign_cook))
            .route("/cooking/shopping-list", get(shopping_list))
            .route("/cooking/api/recipes", get(api_search_recipes))
            .route("/cooking/api/ingredients/suggest", get(api_suggest_ingredients))
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
        "supply_available": true,
    })
}

fn strip_html(s: &str) -> String {
    let mut out = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        match c {
            '<' => in_tag = true,
            '>' => in_tag = false,
            _ if !in_tag => out.push(c),
            _ => {}
        }
    }
    out
}

fn normalize_ingredient(name: &str) -> String {
    let cleaned = strip_html(name);
    cleaned.split_whitespace().collect::<Vec<_>>().join(" ").to_lowercase()
}

fn parse_steps(raw: &str) -> Vec<String> {
    if let Ok(Value::Array(items)) = serde_json::from_str::<Value>(raw) {
        return items
            .iter()
            .filter_map(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(String::from)
            .collect();
    }
    if raw.trim().is_empty() {
        return vec![];
    }
    raw.lines().map(str::trim).filter(|s| !s.is_empty()).map(String::from).collect()
}

/// Split a search query into terms, honoring "quoted phrases".
fn parse_search_terms(query: &str) -> Vec<String> {
    let mut terms = Vec::new();
    let mut remainder = query;
    while let Some(start) = remainder.find('"') {
        if let Some(end_rel) = remainder[start + 1..].find('"') {
            let end = start + 1 + end_rel;
            if end > start + 1 {
                terms.push(remainder[start + 1..end].to_string());
            }
            remainder = &remainder[end + 1..];
        } else {
            break;
        }
    }
    for word in remainder.split_whitespace() {
        terms.push(word.to_string());
    }
    terms.into_iter().filter(|t| !t.is_empty()).collect()
}

fn truncate_chars(s: &str, max: usize) -> (String, bool) {
    if s.chars().count() <= max {
        (s.to_string(), false)
    } else {
        (s.chars().take(max).collect(), true)
    }
}

fn form_map(raw: &[u8]) -> std::collections::HashMap<String, String> {
    form_urlencoded::parse(raw)
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn fv(form: &std::collections::HashMap<String, String>, key: &str) -> Option<String> {
    form.get(key)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

async fn err404(msg: &str) -> Response {
    (axum::http::StatusCode::NOT_FOUND, msg.to_string()).into_response()
}

fn err403(msg: &str) -> Response {
    (axum::http::StatusCode::FORBIDDEN, msg.to_string()).into_response()
}

struct RecipeRow {
    id: String,
    title: String,
    description: Option<String>,
    steps: Option<String>,
    source_link: Option<String>,
    is_public: bool,
    user_id: Option<i64>,
}

fn recipe_row(r: &sqlx::sqlite::SqliteRow) -> RecipeRow {
    RecipeRow {
        id: r.get("id"),
        title: r.get("title"),
        description: r.try_get::<Option<String>, _>("description").ok().flatten(),
        steps: r.try_get::<Option<String>, _>("steps").ok().flatten(),
        source_link: r.try_get::<Option<String>, _>("source_link").ok().flatten(),
        is_public: r.get::<i64, _>("is_public") != 0,
        user_id: r.try_get::<Option<i64>, _>("user_id").ok().flatten(),
    }
}

fn recipe_json(r: &RecipeRow) -> Value {
    json!({
        "id": r.id,
        "title": r.title,
        "description": r.description,
        "is_public": r.is_public,
        "source_link": r.source_link,
    })
}

/// Ingredient display data for a set of recipes: map recipe_id -> Vec<raw_text>.
async fn ingredients_for_recipes(
    db: &syarc_plugin_api::Db,
    recipe_ids: &[String],
) -> std::collections::HashMap<String, Vec<String>> {
    let mut map = std::collections::HashMap::new();
    if recipe_ids.is_empty() {
        return map;
    }
    let placeholders = vec!["?"; recipe_ids.len()].join(",");
    let sql = format!(
        "SELECT ri.recipe_id, ri.raw_text, i.name \
         FROM cooking_recipe_ingredients ri \
         JOIN cooking_ingredients i ON i.id = ri.ingredient_id \
         WHERE ri.recipe_id IN ({placeholders}) ORDER BY ri.rowid"
    );
    let mut q = sqlx::query(&sql);
    for id in recipe_ids {
        q = q.bind(id);
    }
    if let Ok(rows) = q.fetch_all(db).await {
        for r in rows {
            let rid: String = r.get("recipe_id");
            let raw: Option<String> = r.try_get("raw_text").ok().flatten();
            let name: String = r.get("name");
            map.entry(rid).or_default().push(raw.unwrap_or(name));
        }
    }
    map
}

fn can_edit_recipe(user: &User, recipe: &RecipeRow) -> bool {
    user.role == "admin" || recipe.user_id == Some(user.id)
}

/// DISTINCT normalized supply item keys from stores the user owns or is
/// shared into, mirroring the fastmain supply integration.
async fn supply_ingredient_names(db: &syarc_plugin_api::Db, user_id: i64) -> Vec<String> {
    let sql = "\
        SELECT DISTINCT si.key \
        FROM supply_items si \
        JOIN supply_categories sc ON sc.id = si.category_id \
        WHERE (sc.store_id IN (SELECT id FROM supply_stores WHERE user_id = ?) \
               OR sc.store_id IN (SELECT store_id FROM supply_store_shares WHERE shared_with_user_id = ?)) \
        AND si.quantity > 0";
    sqlx::query(sql)
        .bind(user_id)
        .bind(user_id)
        .fetch_all(db)
        .await
        .map(|rows| {
            rows.iter()
                .map(|r| normalize_ingredient(r.get::<String, _>("key").as_str()))
                .collect()
        })
        .unwrap_or_default()
}

fn supply_match(norm: &str, supply_items: &[String]) -> bool {
    supply_items.iter().any(|sn| norm.contains(sn.as_str()) || sn.contains(norm))
}

// ------------------------------------------------------------------- pages

async fn landing(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let db = &state.db;
    let access = "(r.is_public = 1 OR r.user_id = ?)";

    let recipe_count: i64 = sqlx::query_scalar(
        &format!("SELECT COUNT(*) FROM cooking_recipes r WHERE {access}"),
    )
    .bind(user.id)
    .fetch_one(db)
    .await
    .unwrap_or(0);
    let ingredient_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM cooking_ingredients")
            .fetch_one(db)
            .await
            .unwrap_or(0);

    let recent: Vec<RecipeRow> = sqlx::query(
        &format!(
            "SELECT * FROM cooking_recipes r WHERE {access} \
             ORDER BY r.created_at DESC, r.rowid DESC LIMIT 6"
        ),
    )
    .bind(user.id)
    .fetch_all(db)
    .await
    .map(|rows| rows.iter().map(recipe_row).collect())
    .unwrap_or_default();

    let ing_map = ingredients_for_recipes(db, &recent.iter().map(|r| r.id.clone()).collect::<Vec<_>>()).await;

    let recent_recipes: Vec<Value> = recent
        .iter()
        .map(|r| {
            let ings = ing_map.get(&r.id).cloned().unwrap_or_default();
            let preview: Vec<&String> = ings.iter().take(5).collect();
            let (desc_short, truncated) = r
                .description
                .as_deref()
                .map(|d| truncate_chars(d, 150))
                .unwrap_or_default();
            json!({
                "recipe": recipe_json(r),
                "desc_short": desc_short,
                "desc_truncated": truncated,
                "ingredients_preview": preview,
                "ingredient_count": ings.len(),
            })
        })
        .collect();

    let mut ctx = base_ctx(&user);
    ctx["recipe_count"] = json!(recipe_count);
    ctx["ingredient_count"] = json!(ingredient_count);
    ctx["recent_recipes"] = json!(recent_recipes);
    page(&state, "cooking_landing.html", ctx)
}

#[derive(serde::Deserialize, Default)]
struct SearchParams {
    q: Option<String>,
    ingredients: Option<String>,
    filter_by_supply: Option<String>,
    page: Option<i64>,
    per_page: Option<i64>,
}

struct SearchResults {
    recipes: Vec<RecipeRow>,
    total: i64,
    page: i64,
    per_page: i64,
    total_pages: i64,
}

async fn run_search(
    db: &syarc_plugin_api::Db,
    user: &User,
    params: &SearchParams,
) -> SearchResults {
    let query_text = params.q.as_deref().unwrap_or("").trim().to_string();
    let ingredient_list: Vec<String> = params
        .ingredients
        .as_deref()
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect();
    let filter_by_supply = params.filter_by_supply.as_deref() == Some("true");

    let page = params.page.unwrap_or(1).max(1);
    let per_page = params.per_page.unwrap_or(20).clamp(1, 100);

    let mut conditions: Vec<String> = Vec::new();
    let mut binds: Vec<String> = Vec::new();

    if !query_text.is_empty() {
        let terms = parse_search_terms(&query_text);
        if !terms.is_empty() {
            let per_term: Vec<String> = terms
                .iter()
                .map(|t| {
                    binds.push(format!("%{}%", t.to_lowercase()));
                    "(LOWER(r.title) LIKE ? OR LOWER(r.description) LIKE ?)".into()
                })
                .collect();
            // each term binds twice
            let mut all_binds = Vec::new();
            for t in &terms {
                let pat = format!("%{}%", t.to_lowercase());
                all_binds.push(pat.clone());
                all_binds.push(pat);
            }
            binds.truncate(binds.len() - terms.len());
            binds.extend(all_binds);
            conditions.push(per_term.join(" AND "));
        }
    }

    if !ingredient_list.is_empty() {
        let normalized: Vec<String> = ingredient_list.iter().map(|i| normalize_ingredient(i)).collect();
        let placeholders = vec!["?"; normalized.len()].join(",");
        let sql = format!(
            "r.id IN (SELECT ri.recipe_id FROM cooking_recipe_ingredients ri \
             JOIN cooking_ingredients i ON i.id = ri.ingredient_id \
             WHERE i.normalized_name IN ({placeholders}) \
             GROUP BY ri.recipe_id HAVING COUNT(ri.ingredient_id) >= {})",
            normalized.len()
        );
        conditions.push(sql);
        binds.extend(normalized);
    }

    if filter_by_supply {
        let supply_items = supply_ingredient_names(db, user.id).await;
        if !supply_items.is_empty() && ingredient_list.is_empty() {
            let placeholders = vec!["?"; supply_items.len()].join(",");
            conditions.push(format!(
                "r.id IN (SELECT ri.recipe_id FROM cooking_recipe_ingredients ri \
                 JOIN cooking_ingredients i ON i.id = ri.ingredient_id \
                 WHERE i.normalized_name IN ({placeholders}))"
            ));
            binds.extend(supply_items);
        }
    }

    let combined = if conditions.is_empty() {
        String::new()
    } else {
        format!(" AND ({})", conditions.join(" AND "))
    };

    let total: i64 = if !query_text.is_empty() || !ingredient_list.is_empty() {
        let sql = format!(
            "SELECT COUNT(*) FROM cooking_recipes r WHERE (r.is_public = 1 OR r.user_id = ?){combined}"
        );
        let mut q = sqlx::query_scalar::<_, i64>(&sql).bind(user.id);
        for b in &binds {
            q = q.bind(b);
        }
        q.fetch_one(db).await.unwrap_or(0)
    } else {
        0
    };

    let total_pages = if total > 0 { (total + per_page - 1) / per_page } else { 0 };
    let offset = (page - 1) * per_page;

    let recipes: Vec<RecipeRow> = if !query_text.is_empty() || !ingredient_list.is_empty() {
        let sql = format!(
            "SELECT * FROM cooking_recipes r WHERE (r.is_public = 1 OR r.user_id = ?){combined} \
             ORDER BY r.title LIMIT ? OFFSET ?"
        );
        let mut q = sqlx::query(&sql).bind(user.id);
        for b in &binds {
            q = q.bind(b);
        }
        q = q.bind(per_page).bind(offset);
        q.fetch_all(db)
            .await
            .map(|rows| rows.iter().map(recipe_row).collect())
            .unwrap_or_default()
    } else {
        vec![]
    };

    SearchResults { recipes, total, page, per_page, total_pages }
}

fn page_list(page: i64, total_pages: i64) -> Vec<i64> {
    let mut pages: Vec<i64> = Vec::new();
    if total_pages <= 0 {
        return pages;
    }
    let mut last: Option<i64> = None;
    for p in 1..=total_pages {
        if p <= 3 || p > total_pages - 3 || (p >= page - 2 && p <= page + 2) {
            pages.push(p);
            last = Some(p);
        } else if last != Some(-1) && (p == 4 || p == total_pages - 3) {
            pages.push(-1); // ellipsis marker
            last = Some(-1);
        }
    }
    pages
}

async fn search_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<SearchParams>,
) -> Response {
    let db = &state.db;
    let results = run_search(db, &user, &params).await;
    let ing_map =
        ingredients_for_recipes(db, &results.recipes.iter().map(|r| r.id.clone()).collect::<Vec<_>>()).await;

    let recipe_data: Vec<Value> = results
        .recipes
        .iter()
        .map(|r| {
            let ings = ing_map.get(&r.id).cloned().unwrap_or_default();
            let preview: Vec<&String> = ings.iter().take(12).collect();
            let (desc_short, truncated) = r
                .description
                .as_deref()
                .map(|d| truncate_chars(d, 200))
                .unwrap_or_default();
            json!({
                "recipe": recipe_json(r),
                "desc_short": desc_short,
                "desc_truncated": truncated,
                "ingredient_names": preview,
                "ingredient_count": ings.len(),
            })
        })
        .collect();

    let mut ctx = base_ctx(&user);
    ctx["query"] = json!(params.q.as_deref().unwrap_or("").trim());
    ctx["ingredients"] = json!(params.ingredients.as_deref().unwrap_or("").trim());
    ctx["filter_by_supply"] = json!(params.filter_by_supply.as_deref() == Some("true"));
    ctx["recipe_data"] = json!(recipe_data);
    ctx["total"] = json!(results.total);
    ctx["page"] = json!(results.page);
    ctx["total_pages"] = json!(results.total_pages);
    ctx["pages"] = json!(page_list(results.page, results.total_pages));
    page(&state, "cooking_search.html", ctx)
}

async fn new_recipe_page(State(state): State<SharedState>, RequireUser(user): RequireUser) -> Response {
    let mut ctx = base_ctx(&user);
    ctx["mode"] = json!("new");
    ctx["recipe"] = Value::Null;
    ctx["ingredient_texts"] = json!("");
    ctx["steps_text"] = json!("");
    page(&state, "cooking_recipe_form.html", ctx)
}

async fn create_recipe(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    axum::extract::RawForm(raw): axum::extract::RawForm,
) -> Response {
    let db = &state.db;
    let form = form_map(&raw);
    let Some(title) = fv(&form, "title") else {
        return Redirect::to("/cooking/recipes/new?error=Title+is+required.").into_response();
    };
    let ingredients_raw = form.get("ingredients").cloned().unwrap_or_default();

    let recipe_id = uuid::Uuid::new_v4().to_string();
    let res = sqlx::query(
        "INSERT INTO cooking_recipes (id, title, description, steps, source_link, is_public, user_id) \
         VALUES (?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(&recipe_id)
    .bind(title.chars().take(500).collect::<String>())
    .bind(fv(&form, "description"))
    .bind(fv(&form, "steps"))
    .bind(fv(&form, "source_link").map(|s| s.chars().take(500).collect::<String>()))
    .bind(if form.get("is_public").map(String::as_str) == Some("on") { 1 } else { 0 })
    .bind(user.id)
    .execute(db)
    .await;

    if let Err(e) = res {
        return (axum::http::StatusCode::INTERNAL_SERVER_ERROR, format!("failed: {e}")).into_response();
    }

    save_ingredients(db, &recipe_id, &ingredients_raw).await;
    Redirect::to(&format!("/cooking/recipes/{recipe_id}")).into_response()
}

/// Upsert the ingredient index rows for a recipe from newline-separated text.
async fn save_ingredients(db: &syarc_plugin_api::Db, recipe_id: &str, ingredients_raw: &str) {
    for line in ingredients_raw.lines().map(str::trim).filter(|l| !l.is_empty()) {
        let norm = normalize_ingredient(&strip_html(line));
        if norm.len() < 2 {
            continue;
        }
        let ing_id: Option<String> =
            sqlx::query_scalar("SELECT id FROM cooking_ingredients WHERE normalized_name = ?")
                .bind(&norm)
                .fetch_optional(db)
                .await
                .ok()
                .flatten();
        let ing_id = match ing_id {
            Some(id) => id,
            None => {
                let id = uuid::Uuid::new_v4().to_string();
                let name: String = {
                    let mut c = norm.chars();
                    match c.next() {
                        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
                        None => String::new(),
                    }
                };
                if sqlx::query(
                    "INSERT OR IGNORE INTO cooking_ingredients (id, name, normalized_name) VALUES (?, ?, ?)",
                )
                .bind(&id)
                .bind(&name)
                .bind(&norm)
                .execute(db)
                .await
                .is_err()
                {
                    // raced with a concurrent insert; look up the winner
                    match sqlx::query_scalar::<_, String>(
                        "SELECT id FROM cooking_ingredients WHERE normalized_name = ?",
                    )
                    .bind(&norm)
                    .fetch_one(db)
                    .await
                    {
                        Ok(existing) => existing,
                        Err(_) => continue,
                    }
                } else {
                    id
                }
            }
        };
        let _ = sqlx::query(
            "INSERT INTO cooking_recipe_ingredients (id, recipe_id, ingredient_id, raw_text) \
             VALUES (?, ?, ?, ?)",
        )
        .bind(uuid::Uuid::new_v4().to_string())
        .bind(recipe_id)
        .bind(&ing_id)
        .bind(line.chars().take(500).collect::<String>())
        .execute(db)
        .await;
    }
}

async fn fetch_recipe(db: &syarc_plugin_api::Db, id: &str) -> Option<RecipeRow> {
    sqlx::query("SELECT * FROM cooking_recipes r WHERE r.id = ?")
        .bind(id)
        .fetch_optional(db)
        .await
        .ok()
        .flatten()
        .map(|row| recipe_row(&row))
}

#[derive(serde::Deserialize, Default)]
struct RecipeParams {
    q: Option<String>,
    ingredients: Option<String>,
    filter_by_supply: Option<String>,
    page: Option<i64>,
    error: Option<String>,
}

async fn recipe_detail(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(recipe_id): Path<String>,
    Query(params): Query<RecipeParams>,
) -> Response {
    let db = &state.db;
    let Some(recipe) = fetch_recipe(db, &recipe_id).await else {
        return err404("Recipe not found").await;
    };
    if !recipe.is_public && recipe.user_id != Some(user.id) && user.role != "admin" {
        return err403("Access denied");
    }

    let ing_map = ingredients_for_recipes(db, std::slice::from_ref(&recipe_id)).await;
    let ingredient_texts = ing_map.get(&recipe_id).cloned().unwrap_or_default();

    let supply_items = supply_ingredient_names(db, user.id).await;
    let ingredients: Vec<Value> = ingredient_texts
        .iter()
        .map(|t| {
            let norm = normalize_ingredient(t);
            json!({ "text": t, "in_supply": supply_match(&norm, &supply_items) })
        })
        .collect();

    let steps_list: Vec<String> = recipe.steps.as_deref().map(parse_steps).unwrap_or_default();
    let similar: Vec<&String> = ingredient_texts.iter().take(5).collect();

    let mut ctx = base_ctx(&user);
    ctx["recipe"] = recipe_json(&recipe);
    ctx["ingredients"] = json!(ingredients);
    ctx["steps_list"] = json!(steps_list);
    ctx["similar"] = json!(similar);
    ctx["search_q"] = json!(params.q.as_deref().unwrap_or(""));
    ctx["search_ingredients"] = json!(params.ingredients.as_deref().unwrap_or(""));
    ctx["search_filter_by_supply"] = json!(params.filter_by_supply.as_deref() == Some("true"));
    ctx["search_page"] = json!(params.page.unwrap_or(1));
    ctx["can_edit"] = json!(can_edit_recipe(&user, &recipe));
    ctx["error"] = json!(params.error.as_deref().unwrap_or(""));
    page(&state, "cooking_recipe.html", ctx)
}

async fn edit_recipe_page(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(recipe_id): Path<String>,
) -> Response {
    let db = &state.db;
    let Some(recipe) = fetch_recipe(db, &recipe_id).await else {
        return err404("Recipe not found").await;
    };
    if !can_edit_recipe(&user, &recipe) {
        return err403("Access denied");
    }
    let ing_map = ingredients_for_recipes(db, std::slice::from_ref(&recipe_id)).await;
    let ingredient_texts = ing_map.get(&recipe_id).cloned().unwrap_or_default().join("\n");
    let steps_text = recipe.steps.as_deref().map(parse_steps).unwrap_or_default().join("\n");

    let mut ctx = base_ctx(&user);
    ctx["mode"] = json!("edit");
    ctx["recipe"] = recipe_json(&recipe);
    ctx["ingredient_texts"] = json!(ingredient_texts);
    ctx["steps_text"] = json!(steps_text);
    page(&state, "cooking_recipe_form.html", ctx)
}

async fn update_recipe(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(recipe_id): Path<String>,
    axum::extract::RawForm(raw): axum::extract::RawForm,
) -> Response {
    let db = &state.db;
    let Some(recipe) = fetch_recipe(db, &recipe_id).await else {
        return err404("Recipe not found").await;
    };
    if !can_edit_recipe(&user, &recipe) {
        return err403("Access denied");
    }

    let form = form_map(&raw);
    let Some(title) = fv(&form, "title") else {
        return Redirect::to(&format!("/cooking/recipes/{recipe_id}/edit?error=Title+is+required."))
            .into_response();
    };

    let _ = sqlx::query(
        "UPDATE cooking_recipes SET title = ?, description = ?, steps = ?, source_link = ?, is_public = ? \
         WHERE id = ?",
    )
    .bind(title.chars().take(500).collect::<String>())
    .bind(fv(&form, "description"))
    .bind(fv(&form, "steps"))
    .bind(fv(&form, "source_link").map(|s| s.chars().take(500).collect::<String>()))
    .bind(if form.get("is_public").map(String::as_str) == Some("on") { 1 } else { 0 })
    .bind(&recipe_id)
    .execute(db)
    .await;

    let _ = sqlx::query("DELETE FROM cooking_recipe_ingredients WHERE recipe_id = ?")
        .bind(&recipe_id)
        .execute(db)
        .await;
    save_ingredients(db, &recipe_id, form.get("ingredients").map(String::as_str).unwrap_or("")).await;

    Redirect::to(&format!("/cooking/recipes/{recipe_id}")).into_response()
}

async fn delete_recipe(
    State(_state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(recipe_id): Path<String>,
) -> Response {
    let db = &_state.db;
    let Some(recipe) = fetch_recipe(db, &recipe_id).await else {
        return err404("Recipe not found").await;
    };
    if !can_edit_recipe(&user, &recipe) {
        return err403("Access denied");
    }
    let _ = sqlx::query("DELETE FROM cooking_recipes WHERE id = ?")
        .bind(&recipe_id)
        .execute(db)
        .await;
    Redirect::to("/cooking/").into_response()
}

async fn add_to_menu(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(recipe_id): Path<String>,
    axum::extract::RawForm(raw): axum::extract::RawForm,
) -> Response {
    let db = &state.db;
    let Some(recipe) = fetch_recipe(db, &recipe_id).await else {
        return err404("Recipe not found").await;
    };
    if !recipe.is_public && recipe.user_id != Some(user.id) && user.role != "admin" {
        return err403("Access denied");
    }

    let form = form_map(&raw);
    let Some(menu_date) = fv(&form, "menu_date") else {
        return Redirect::to(&format!(
            "/cooking/recipes/{recipe_id}?error=A+date+is+required+to+add+to+the+menu."
        ))
        .into_response();
    };
    let Ok(day) = chrono::NaiveDate::parse_from_str(&menu_date, "%Y-%m-%d") else {
        return Redirect::to(&format!("/cooking/recipes/{recipe_id}?error=Invalid+date.")).into_response();
    };
    let mut meal_type = fv(&form, "meal_type").unwrap_or_else(|| "dinner".into());
    if !MEAL_TYPES.contains(&meal_type.as_str()) {
        meal_type = "dinner".into();
    }

    let _ = sqlx::query(
        "INSERT INTO cooking_menu_entries (id, recipe_id, menu_date, meal_type, user_id) \
         VALUES (?, ?, ?, ?, ?)",
    )
    .bind(uuid::Uuid::new_v4().to_string())
    .bind(&recipe_id)
    .bind(day.format("%Y-%m-%d").to_string())
    .bind(&meal_type)
    .bind(user.id)
    .execute(db)
    .await;

    Redirect::to(&format!("/cooking/menu?year={}&month={}", day.format("%Y"), day.format("%m"))).into_response()
}

const MONTH_NAMES: [&str; 12] = [
    "January", "February", "March", "April", "May", "June",
    "July", "August", "September", "October", "November", "December",
];

async fn menu_calendar(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<std::collections::HashMap<String, String>>,
) -> Response {
    let db = &state.db;
    let today = chrono::Local::now().date_naive();
    let year: i32 = params
        .get("year")
        .and_then(|v| v.parse().ok())
        .unwrap_or(today.format("%Y").to_string().parse().unwrap_or(2026));
    let month: u32 = params
        .get("month")
        .and_then(|v| v.parse::<u32>().ok())
        .unwrap_or_else(|| today.format("%m").to_string().parse().unwrap_or(1))
        .clamp(1, 12);

    let num_days = days_in_month(year, month);
    use chrono::Datelike;
    // Monday-first weekday of the 1st
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap_or(today);
    let first_weekday = first.weekday().num_days_from_monday();

    let start = format!("{year:04}-{month:02}-01");
    let end = format!("{year:04}-{month:02}-{num_days:02}");
    let rows = sqlx::query(
        "SELECT m.id, m.recipe_id, m.menu_date, m.meal_type, r.title, m.user_id, m.cook_user_id, cu.username AS cook \
         FROM cooking_menu_entries m \
         JOIN cooking_recipes r ON r.id = m.recipe_id \
         LEFT JOIN users cu ON cu.id = m.cook_user_id \
         WHERE (r.is_public = 1 OR r.user_id = ?) AND m.menu_date >= ? AND m.menu_date <= ? \
         ORDER BY m.menu_date, m.meal_type",
    )
    .bind(user.id)
    .bind(&start)
    .bind(&end)
    .fetch_all(db)
    .await
    .unwrap_or_default();

    use std::collections::BTreeMap;
    let mut cal: BTreeMap<String, Vec<Value>> = BTreeMap::new();
    let mut delete_allowed: std::collections::HashMap<String, bool> = std::collections::HashMap::new();
    for r in &rows {
        let date: String = r.get("menu_date");
        let entry_id: String = r.get("id");
        let recipe_id: String = r.get("recipe_id");
        let title: String = r.get("title");
        let meal: String = r.get("meal_type");
        let cook: Option<String> = r.try_get::<Option<String>, _>("cook").ok().flatten();
        let cook_user_id: Option<i64> = r.try_get::<Option<i64>, _>("cook_user_id").ok().flatten();
        let entry_user: Option<i64> = r.try_get("user_id").ok().flatten();
        let recipe_owner: Option<i64> = r.try_get::<Option<i64>, _>("user_id").ok().flatten();
        let _ = recipe_owner;
        delete_allowed.insert(
            entry_id.clone(),
            user.role == "admin" || entry_user == Some(user.id),
        );
        cal.entry(date.clone()).or_default().push(json!({
            "id": entry_id,
            "recipe_id": recipe_id,
            "title": title,
            "meal_type": meal,
            "cook": cook,
            "cook_user_id": cook_user_id,
        }));
    }

    // Precompute the calendar grid: 6 weeks x 7 cells, Monday-first.
    let today_str = today.format("%Y-%m-%d").to_string();
    let mut weeks: Vec<Vec<Value>> = Vec::new();
    let mut cell = 0i64;
    for _ in 0..6 {
        let mut week: Vec<Value> = Vec::new();
        for _ in 0..7 {
            cell += 1;
            let day = cell - first_weekday as i64;
            if cell <= first_weekday as i64 || day > num_days as i64 || day < 1 {
                week.push(Value::Null);
                continue;
            }
            let date = format!("{year:04}-{month:02}-{day:02}");
            let entries: Vec<Value> = cal
                .get(&date)
                .cloned()
                .unwrap_or_default()
                .into_iter()
                .map(|mut e| {
                    let id = e["id"].as_str().unwrap_or("").to_string();
                    e["can_delete"] = json!(delete_allowed.get(&id).copied().unwrap_or(false));
                    e
                })
                .collect();
            week.push(json!({
                "day": day,
                "date": date,
                "is_today": date == today_str,
                "entries": entries,
            }));
        }
        let last_day = cell - first_weekday as i64;
        weeks.push(week);
        if last_day >= num_days as i64 {
            break;
        }
    }

    let (prev_year, prev_month) = if month > 1 { (year, month - 1) } else { (year - 1, 12) };
    let (next_year, next_month) = if month < 12 { (year, month + 1) } else { (year + 1, 1) };

    let mut ctx = base_ctx(&user);
    ctx["year"] = json!(year);
    ctx["month"] = json!(month);
    ctx["month_name"] = json!(MONTH_NAMES[(month - 1) as usize]);
    ctx["weeks"] = json!(weeks);
    ctx["num_days"] = json!(num_days);
    ctx["month_padded"] = json!(format!("{month:02}"));
    ctx["days_padded"] = json!(format!("{num_days:02}"));
    ctx["meal_types"] = json!(MEAL_TYPES);
    ctx["users"] = json!(
        sqlx::query("SELECT id, username FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%' ORDER BY username")
            .fetch_all(db).await.unwrap_or_default()
            .iter().map(|r| json!({"id": r.get::<i64, _>("id"), "username": r.get::<String, _>("username")}))
            .collect::<Vec<_>>()
    );
    ctx["prev_year"] = json!(prev_year);
    ctx["prev_month"] = json!(prev_month);
    ctx["prev_month_name"] = json!(MONTH_NAMES[(prev_month - 1) as usize]);
    ctx["next_year"] = json!(next_year);
    ctx["next_month"] = json!(next_month);
    ctx["next_month_name"] = json!(MONTH_NAMES[(next_month - 1) as usize]);
    page(&state, "cooking_menu.html", ctx)
}

fn days_in_month(year: i32, month: u32) -> u32 {
    let (y, m) = if month == 12 { (year + 1, 1) } else { (year, month + 1) };
    let next = chrono::NaiveDate::from_ymd_opt(y, m, 1).unwrap();
    let first = chrono::NaiveDate::from_ymd_opt(year, month, 1).unwrap();
    (next - first).num_days() as u32
}

async fn delete_menu_entry(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(entry_id): Path<String>,
) -> Response {
    let db = &state.db;
    let row = sqlx::query(
        "SELECT m.id, m.menu_date, m.user_id, r.user_id AS recipe_user_id \
         FROM cooking_menu_entries m JOIN cooking_recipes r ON r.id = m.recipe_id \
         WHERE m.id = ?",
    )
    .bind(&entry_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let Some(row) = row else {
        return err404("Menu entry not found").await;
    };
    let entry_user: Option<i64> = row.try_get("user_id").ok().flatten();
    let recipe_user: Option<i64> = row.try_get("recipe_user_id").ok().flatten();
    let menu_date: String = row.get("menu_date");
    if !(user.role == "admin" || entry_user == Some(user.id) || recipe_user == Some(user.id)) {
        return err403("Access denied");
    }
    let _ = sqlx::query("DELETE FROM cooking_menu_entries WHERE id = ?")
        .bind(&entry_id)
        .execute(db)
    .await;
    let mut parts = menu_date.split('-');
    let (y, m) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    Redirect::to(&format!("/cooking/menu?year={y}&month={m}")).into_response()
}

/// Assign (or clear) the cook for a menu entry. Any active user can volunteer;
/// the entry owner / recipe owner / an admin can assign anyone.
async fn assign_cook(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Path(entry_id): Path<String>,
    body: axum::body::Bytes,
) -> Response {
    let db = &state.db;
    let form = form_map(&body);
    let row = sqlx::query(
        "SELECT m.menu_date, m.user_id, m.cook_user_id, r.user_id AS recipe_user_id \
         FROM cooking_menu_entries m JOIN cooking_recipes r ON r.id = m.recipe_id \
         WHERE m.id = ?",
    )
    .bind(&entry_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten();
    let Some(row) = row else {
        return err404("Menu entry not found").await;
    };
    let entry_user: Option<i64> = row.try_get("user_id").ok().flatten();
    let recipe_user: Option<i64> = row.try_get("recipe_user_id").ok().flatten();
    let can_assign_any = user.role == "admin" || entry_user == Some(user.id) || recipe_user == Some(user.id);

    let cook_id: Option<i64> = form.get("cook_user_id").and_then(|v| v.parse().ok());
    let cook_id = match cook_id {
        Some(id) if id == user.id => Some(id),
        Some(id) if can_assign_any => Some(id),
        _ if form.contains_key("cook_user_id") && form.get("cook_user_id").map(String::is_empty).unwrap_or(false) => {
            // clearing: only assigners may clear someone else's assignment
            let current: Option<i64> = row.try_get("cook_user_id").ok().flatten();
            if current.is_none() || current == Some(user.id) || can_assign_any {
                None
            } else {
                return err403("Access denied");
            }
        }
        _ => return err403("Access denied"),
    };
    let _ = sqlx::query("UPDATE cooking_menu_entries SET cook_user_id = ? WHERE id = ?")
        .bind(cook_id)
        .bind(&entry_id)
        .execute(db)
        .await;
    let menu_date: String = row.get("menu_date");
    let mut parts = menu_date.split('-');
    let (y, m) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
    Redirect::to(&format!("/cooking/menu?year={y}&month={m}")).into_response()
}

#[derive(serde::Deserialize, Default)]
struct ShoppingParams {
    start_date: Option<String>,
    end_date: Option<String>,
}

fn fmt_short(date: &str) -> String {
    chrono::NaiveDate::parse_from_str(date, "%Y-%m-%d")
        .map(|d| d.format("%b %d, %Y").to_string())
        .unwrap_or_else(|_| date.to_string())
}

async fn shopping_list(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<ShoppingParams>,
) -> Response {
    let db = &state.db;
    let start = params.start_date.as_deref().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let end = params.end_date.as_deref().and_then(|s| chrono::NaiveDate::parse_from_str(s, "%Y-%m-%d").ok());
    let parse_error = (params.start_date.is_some() && start.is_none())
        || (params.end_date.is_some() && end.is_none());

    let mut recipes_included: Vec<Value> = Vec::new();
    let mut aggregated: std::collections::BTreeMap<String, Vec<String>> = std::collections::BTreeMap::new();

    if let (Some(start), Some(end)) = (start, end) {
        if !parse_error && start <= end {
            let rows = sqlx::query(
                "SELECT m.menu_date, m.meal_type, r.id AS recipe_id, r.title \
                 FROM cooking_menu_entries m \
                 JOIN cooking_recipes r ON r.id = m.recipe_id \
                 WHERE (r.is_public = 1 OR r.user_id = ?) AND m.menu_date >= ? AND m.menu_date <= ? \
                 ORDER BY m.menu_date, m.meal_type",
            )
            .bind(user.id)
            .bind(start.format("%Y-%m-%d").to_string())
            .bind(end.format("%Y-%m-%d").to_string())
            .fetch_all(db)
            .await
            .unwrap_or_default();

            let ids: Vec<String> = rows.iter().map(|r| r.get::<String, _>("recipe_id")).collect();
            let ing_map = ingredients_for_recipes(db, &ids).await;

            for r in &rows {
                let recipe_id: String = r.get("recipe_id");
                let menu_date: String = r.get("menu_date");
                recipes_included.push(json!({
                    "id": recipe_id,
                    "title": r.get::<String, _>("title"),
                    "menu_date": menu_date,
                    "menu_date_short": chrono::NaiveDate::parse_from_str(&menu_date, "%Y-%m-%d")
                        .map(|d| d.format("%b %d").to_string())
                        .unwrap_or_else(|_| menu_date.clone()),
                    "meal_type": r.get::<String, _>("meal_type"),
                }));
                for raw in ing_map.get(&recipe_id).cloned().unwrap_or_default() {
                    let norm = normalize_ingredient(&strip_html(&raw));
                    if norm.len() < 2 {
                        continue;
                    }
                    let entry = aggregated.entry(norm).or_default();
                    if !entry.contains(&raw) {
                        entry.push(raw);
                    }
                }
            }
        }
    }

    let supply_items = supply_ingredient_names(db, user.id).await;
    let mut need_to_buy: Vec<Value> = Vec::new();
    let mut in_supply: Vec<Value> = Vec::new();
    for (norm, raws) in &aggregated {
        let name = capitalize(norm);
        let item = json!({ "name": name, "raw_texts": raws });
        if supply_match(norm, &supply_items) {
            in_supply.push(item);
        } else {
            need_to_buy.push(item);
        }
    }
    need_to_buy.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));
    in_supply.sort_by(|a, b| a["name"].as_str().cmp(&b["name"].as_str()));

    let mut ctx = base_ctx(&user);
    ctx["start_date"] = json!(params.start_date.as_deref().unwrap_or(""));
    ctx["end_date"] = json!(params.end_date.as_deref().unwrap_or(""));
    ctx["start_str"] = json!(params.start_date.as_deref().map(fmt_short));
    ctx["end_str"] = json!(params.end_date.as_deref().map(fmt_short));
    ctx["parse_error"] = json!(parse_error);
    ctx["has_range"] = json!(start.is_some() && end.is_some() && !parse_error);
    ctx["need_to_buy"] = json!(need_to_buy);
    ctx["in_supply"] = json!(in_supply);
    ctx["recipes_included"] = json!(recipes_included);
    page(&state, "cooking_shopping_list.html", ctx)
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

// ------------------------------------------------------------------ api

async fn api_search_recipes(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    Query(params): Query<SearchParams>,
) -> Response {
    let db = &state.db;
    let results = run_search(db, &user, &params).await;
    let ing_map =
        ingredients_for_recipes(db, &results.recipes.iter().map(|r| r.id.clone()).collect::<Vec<_>>()).await;
    let items: Vec<Value> = results
        .recipes
        .iter()
        .map(|r| {
            json!({
                "id": r.id,
                "title": r.title,
                "description": r.description,
                "ingredients": ing_map.get(&r.id).cloned().unwrap_or_default(),
            })
        })
        .collect();
    axum::Json(json!({
        "items": items,
        "total": results.total,
        "page": results.page,
        "per_page": results.per_page,
        "total_pages": results.total_pages,
    }))
    .into_response()
}

#[derive(serde::Deserialize)]
struct SuggestParams {
    q: Option<String>,
    limit: Option<i64>,
}

async fn api_suggest_ingredients(
    State(state): State<SharedState>,
    RequireUser(_user): RequireUser,
    Query(params): Query<SuggestParams>,
) -> Response {
    let query = params.q.as_deref().unwrap_or("").trim().to_lowercase();
    if query.is_empty() {
        return axum::Json(json!({ "suggestions": [] })).into_response();
    }
    let limit = params.limit.unwrap_or(20).clamp(1, 50);
    let names: Vec<String> = sqlx::query(
        "SELECT name FROM cooking_ingredients WHERE normalized_name LIKE ? \
         ORDER BY name LIMIT ?",
    )
    .bind(format!("%{query}%"))
    .bind(limit)
    .fetch_all(&state.db)
    .await
    .map(|rows| rows.iter().map(|r| r.get("name")).collect())
    .unwrap_or_default();
    axum::Json(json!({ "suggestions": names })).into_response()
}
