use std::sync::OnceLock;

use minijinja::Environment;

pub struct Templates {
    env: Environment<'static>,
}

fn create_env() -> Environment<'static> {
    let mut env = Environment::new();

    fn load_dir(env: &mut Environment<'static>, dir: &str) {
        load_dir_inner(env, dir, dir);
    }

    fn load_dir_inner(env: &mut Environment<'static>, dir: &str, root: &str) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                load_dir_inner(env, &path.to_string_lossy(), root);
                continue;
            }
            if path.extension().and_then(|e| e.to_str()) == Some("html") {
                let name = path
                    .strip_prefix(root)
                    .ok()
                    .and_then(|r| r.to_str())
                    .map(|r| r.trim_start_matches('/').to_string())
                    .or_else(|| path.file_name().and_then(|s| s.to_str().map(String::from)));
                if let Some(name) = name {
                    if let Ok(contents) = std::fs::read_to_string(&path) {
                        env.add_template_owned(name, contents).ok();
                    }
                }
            }
        }
    }

    load_dir(&mut env, "crates/core/src/templates");
    load_dir(&mut env, "apps/admin/src/templates");
    load_dir(&mut env, "apps/platforms/src/templates");
    load_dir(&mut env, "apps/surveillance/src/templates");
    load_dir(&mut env, "apps/supply/src/templates");
    load_dir(&mut env, "apps/pecuniary/src/templates");
    load_dir(&mut env, "apps/library/src/templates");
    load_dir(&mut env, "apps/agriculture/src/templates");
    load_dir(&mut env, "apps/conferencing/src/templates");
    load_dir(&mut env, "apps/hangman/src/templates");
    load_dir(&mut env, "apps/wordgame/src/templates");
    load_dir(&mut env, "apps/cooking/src/templates");
    load_dir(&mut env, "apps/meshlink/src/templates");
    load_dir(&mut env, "apps/calendar/src/templates");
    load_dir(&mut env, "templates");

    env.add_filter("urlencode", |value: String| -> String {
        let mut out = String::new();
        for byte in value.bytes() {
            match byte {
                b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                    out.push(byte as char)
                }
                _ => out.push_str(&format!("%{byte:02X}")),
            }
        }
        out
    });

    env.add_filter("num", |value: minijinja::value::Value, decimals: Option<usize>| -> String {
        let decimals = decimals.unwrap_or(2);
        let n = f64::try_from(value.clone()).ok();
        match n {
            Some(n) => {
                let s = format!("{n:.decimals$}");
                let (int, frac) = s.split_once('.').unwrap_or((s.as_str(), ""));
                let negative = int.starts_with('-');
                let digits = int.trim_start_matches('-');
                let mut grouped = String::new();
                for (i, c) in digits.chars().enumerate() {
                    if i > 0 && (digits.len() - i) % 3 == 0 {
                        grouped.push(',');
                    }
                    grouped.push(c);
                }
                let mut out = String::new();
                if negative { out.push('-'); }
                out.push_str(&grouped);
                if decimals > 0 {
                    out.push('.');
                    out.push_str(frac);
                }
                out
            }
            None => value.to_string(),
        }
    });

    env.add_filter("localtime", |value: String, fmt: Option<String>| -> String {
        let fmt = fmt.as_deref().unwrap_or("%b %d, %Y %H:%M");
        let parsed = chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%d %H:%M:%S%.f")
            .or_else(|_| chrono::NaiveDateTime::parse_from_str(&value, "%Y-%m-%dT%H:%M:%S%.fZ"));
        match parsed {
            Ok(naive) => {
                use chrono::TimeZone;
                chrono::Local.from_utc_datetime(&naive).format(fmt).to_string()
            }
            Err(_) => value,
        }
    });

    env
}

impl Templates {
    pub fn load() -> Result<Self, minijinja::Error> {
        static ENV: OnceLock<Environment<'static>> = OnceLock::new();
        let env = ENV.get_or_init(create_env).clone();
        Ok(Self { env })
    }

    pub fn render(&self, name: &str, ctx: serde_json::Value) -> String {
        let ctx: minijinja::Value =
            serde_json::from_value(ctx).expect("template context serialization failed");
        match self
            .env
            .get_template(name)
            .and_then(|t| t.render(ctx))
        {
            Ok(html) => html,
            Err(e) => {
                eprintln!("template render error ({name}): {e}");
                format!("<h1>Template error: {name}</h1>")
            }
        }
    }
}
