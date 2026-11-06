use syarc_plugin_api::{AppPlugin, Db};

/// Compile-time app registry. Adding an app = one crate + one line here.
pub fn all_apps() -> Vec<Box<dyn AppPlugin>> {
    vec![
        Box::new(syarc_admin::AdminPlugin),
        Box::new(syarc_platforms::PlatformsPlugin),
        Box::new(syarc_surveillance::SurveillancePlugin),
        Box::new(syarc_supply::SupplyPlugin),
        Box::new(syarc_pecuniary::PecuniaryPlugin),
        Box::new(syarc_library::LibraryPlugin),
        Box::new(syarc_agriculture::AgriculturePlugin),
        Box::new(syarc_agriculture::AgronomyPlugin),
        Box::new(syarc_agriculture::HusbandryPlugin),
        Box::new(syarc_agriculture::OrchardPlugin),
        Box::new(syarc_agriculture::GardenPlugin),
        Box::new(syarc_agriculture::EggsPlugin),
        Box::new(syarc_agriculture::MilkPlugin),
        Box::new(syarc_agriculture::MeatPlugin),
        Box::new(syarc_agriculture::ForagingPlugin),
        Box::new(syarc_conferencing::ConferencingPlugin),
        Box::new(syarc_hangman::HangmanPlugin),
        Box::new(syarc_cooking::CookingPlugin),
        Box::new(syarc_meshlink::MeshlinkPlugin),
        Box::new(syarc_wordgame::WordgamePlugin),
        Box::new(syarc_calendar::CalendarPlugin),
    ]
}

/// Seed app rows from manifests without clobbering admin-assigned categories,
/// mirroring AppPlugin::on_startup in the Python version.
pub async fn seed_apps(db: &Db, plugins: &[Box<dyn AppPlugin>]) -> Result<(), sqlx::Error> {
    for plugin in plugins {
        let m = plugin.manifest();
        let category_id: Option<i64> = match m.category {
            Some(name) => {
                let slug = slugify(name);
                sqlx::query(
                    "INSERT INTO app_categories (name, slug) VALUES (?, ?)
                     ON CONFLICT(slug) DO UPDATE SET name = excluded.name",
                )
                .bind(name)
                .bind(&slug)
                .execute(db)
                .await?;
                sqlx::query_scalar::<_, i64>(
                    "SELECT id FROM app_categories WHERE slug = ?",
                )
                .bind(&slug)
                .fetch_one(db)
                .await
                .map(Some)?
            }
            None => None,
        };

        let existing: Option<String> =
            sqlx::query_scalar("SELECT id FROM apps WHERE id = ?")
                .bind(m.id)
                .fetch_optional(db)
                .await?;

        if existing.is_some() {
            sqlx::query(
                "UPDATE apps SET name = ?, url_prefix = ?, icon_class = ?, is_active = 1,
                 show_on_dashboard = ?,
                 category_id = COALESCE(category_id, ?)
                 WHERE id = ?",
            )
            .bind(m.name)
            .bind(m.url_prefix)
            .bind(m.icon_class)
            .bind(m.show_on_dashboard)
            .bind(category_id)
            .bind(m.id)
            .execute(db)
            .await?;
        } else {
            sqlx::query(
                "INSERT INTO apps (id, name, url_prefix, icon_class, category_id, show_on_dashboard)
                 VALUES (?, ?, ?, ?, ?, ?)",
            )
            .bind(m.id)
            .bind(m.name)
            .bind(m.url_prefix)
            .bind(m.icon_class)
            .bind(category_id)
            .bind(m.show_on_dashboard)
            .execute(db)
            .await?;
        }
    }
    Ok(())
}

/// Deactivate app rows whose plugin no longer exists.
pub async fn reconcile_apps(db: &Db, plugins: &[Box<dyn AppPlugin>]) -> Result<(), sqlx::Error> {    let ids: Vec<String> = plugins.iter().map(|p| p.manifest().id.to_string()).collect();
    sqlx::query("UPDATE apps SET is_active = 0 WHERE is_active = 1")
        .execute(db)
        .await?;
    if ids.is_empty() {
        return Ok(());
    }
    let placeholders = vec!["?"; ids.len()].join(",");
    let sql = format!(
        "UPDATE apps SET is_active = 1 WHERE id IN ({placeholders})"
    );
    let mut q = sqlx::query(&sql);
    for id in &ids {
        q = q.bind(id);
    }
    q.execute(db).await?;
    Ok(())
}

/// First-run default: grant every app to every active human user so existing
/// installs keep working until an admin edits permissions. Mesh-* service
/// accounts (auto-provisioned radio bindings, never log in) are excluded.
pub async fn seed_default_permissions(
    db: &Db,
    plugins: &[Box<dyn AppPlugin>],
) -> Result<(), sqlx::Error> {
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM user_app_permissions")
        .fetch_one(db)
        .await?;
    if count > 0 {
        return Ok(());
    }
    for plugin in plugins {
        sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id) \
             SELECT id, ? FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%'",
        )
        .bind(plugin.manifest().id)
        .execute(db)
        .await?;
    }
    Ok(())
}

/// True if the user may reach the given app. Admins bypass; mesh service
/// accounts (which never log in) are always allowed to keep the broker
/// bindings working.
pub async fn user_can_access_app(
    db: &Db,
    user_id: i64,
    role: &str,
    app_id: &str,
) -> bool {
    if role == "admin" {
        return true;
    }
    sqlx::query_scalar::<_, i64>(
        "SELECT 1 FROM user_app_permissions WHERE user_id = ? AND app_id = ?",
    )
    .bind(user_id)
    .bind(app_id)
    .fetch_optional(db)
    .await
    .ok()
    .flatten()
    .is_some()
}

fn slugify(text: &str) -> String {
    let slug: String = text
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect::<String>()
        .to_lowercase();
    let trimmed = slug.trim_matches('-').to_string();
    if trimmed.is_empty() { "uncategorized".into() } else { trimmed }
}
