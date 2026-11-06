//! Word Game: a Word Chums / Scrabble-style tile game for two players.
//! Ported from the fastmain (FastAPI) wordgame app with rule fixes:
//! first move must cover the center square, later moves must connect to
//! existing tiles, blank tiles are omitted, and consecutive passes end
//! the game. Polling-based (no WS); dictionary embedded at compile time.

use std::sync::OnceLock;

use axum::extract::{Path as AxPath, State};
use axum::response::{Html, IntoResponse, Response};
use axum::routing::{get, post};
use axum::Router;
use rand::seq::SliceRandom;
use serde::Deserialize;
use serde_json::{json, Value};
use sqlx::Row;

use syarc_plugin_api::{RequireUser, SharedState};

const DICT: &str = include_str!("../scrabble_dictionary.txt");
pub const BOARD_SIZE: usize = 15;
const RACK_SIZE: usize = 7;

// ---------------------------------------------------------------------------
// Dictionary

pub fn dictionary() -> &'static std::collections::HashSet<String> {
    static D: OnceLock<std::collections::HashSet<String>> = OnceLock::new();
    D.get_or_init(|| {
        let mut set: std::collections::HashSet<String> = DICT
            .lines()
            .map(|l| l.trim().to_uppercase())
            .filter(|l| l.len() >= 2 && l.chars().all(|c| c.is_ascii_alphabetic()))
            .collect();
        if set.is_empty() {
            for w in ["AT", "IT", "AN", "WORD", "GAME", "TILE"] {
                set.insert(w.to_string());
            }
        }
        set
    })
}

// ---------------------------------------------------------------------------
// Tiles / board

fn letter_points(c: char) -> i64 {
    match c {
        'A' | 'E' | 'I' | 'O' | 'U' | 'L' | 'N' | 'S' | 'T' | 'R' => 1,
        'D' | 'G' => 2,
        'B' | 'C' | 'M' | 'P' => 3,
        'F' | 'H' | 'V' | 'W' | 'Y' => 4,
        'K' => 5,
        'J' | 'X' => 8,
        'Q' | 'Z' => 10,
        _ => 0,
    }
}

/// (row, col, letter-multiplier, word-multiplier)
fn bonus_squares() -> &'static Vec<(usize, usize, i64, i64)> {
    static B: OnceLock<Vec<(usize, usize, i64, i64)>> = OnceLock::new();
    B.get_or_init(|| {
        let mut v = Vec::new();
        let tw = [(0, 0), (0, 7), (0, 14), (7, 0), (7, 14), (14, 0), (14, 7), (14, 14)];
        let tl = [
            (1, 5), (1, 9), (5, 1), (5, 5), (5, 9), (5, 13), (9, 1), (9, 5), (9, 9), (9, 13),
            (13, 5), (13, 9),
        ];
        let dw_d1 = [(1, 1), (2, 2), (3, 3), (4, 4), (10, 10), (11, 11), (12, 12), (13, 13)];
        let dw_d2 = [(1, 13), (2, 12), (3, 11), (4, 10), (10, 4), (11, 3), (12, 2), (13, 1)];
        let dl = [
            (0, 3), (0, 11), (2, 6), (2, 8), (3, 0), (3, 7), (3, 14), (6, 2), (6, 6), (6, 8),
            (6, 12), (7, 3), (7, 11), (8, 2), (8, 6), (8, 8), (8, 12), (11, 0), (11, 7),
            (11, 14), (12, 6), (12, 8), (14, 3), (14, 11),
        ];
        for (r, c) in tw {
            v.push((r, c, 1, 3));
        }
        for (r, c) in tl {
            v.push((r, c, 3, 1));
        }
        for (r, c) in dw_d1.into_iter().chain(dw_d2) {
            v.push((r, c, 1, 2));
        }
        // center (7,7) is a DW too
        v.push((7, 7, 1, 2));
        for (r, c) in dl {
            v.push((r, c, 2, 1));
        }
        v
    })
}

pub fn bonus_at(row: usize, col: usize) -> (i64, i64) {
    bonus_squares()
        .iter()
        .find(|(r, c, _, _)| *r == row && *c == col)
        .map(|(_, _, l, w)| (*l, *w))
        .unwrap_or((1, 1))
}

pub fn fresh_bag() -> Vec<char> {
    let dist: &[(&str, usize)] = &[
        ("A", 9), ("B", 2), ("C", 2), ("D", 4), ("E", 12), ("F", 2), ("G", 3), ("H", 2),
        ("I", 9), ("J", 1), ("K", 1), ("L", 4), ("M", 2), ("N", 6), ("O", 8), ("P", 2),
        ("Q", 1), ("R", 6), ("S", 4), ("T", 6), ("U", 4), ("V", 2), ("W", 2), ("X", 1),
        ("Y", 2), ("Z", 1),
    ];
    let mut bag: Vec<char> = Vec::new();
    for (letter, count) in dist {
        for _ in 0..*count {
            bag.push(letter.chars().next().unwrap());
        }
    }
    bag.shuffle(&mut rand::thread_rng());
    bag
}

/// 15x15 board of empty strings / single-char letters.
pub type Board = Vec<Vec<String>>;

fn empty_board() -> Board {
    vec![vec![String::new(); BOARD_SIZE]; BOARD_SIZE]
}

fn parse_board(s: &str) -> Board {
    serde_json::from_str::<Board>(s).unwrap_or_else(|_| empty_board())
}

fn parse_tiles(s: &str) -> Vec<char> {
    serde_json::from_str::<Vec<String>>(s)
        .unwrap_or_default()
        .iter()
        .filter_map(|t| t.chars().next())
        .collect()
}

fn now_str() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.f").to_string()
}

fn new_uuid() -> String {
    uuid::Uuid::new_v4().to_string()
}

// ---------------------------------------------------------------------------
// Move validation / scoring

#[derive(Deserialize)]
struct TilePlacement {
    letter: String,
    row: usize,
    col: usize,
}

pub struct Placed {
    pub letter: char,
    pub row: usize,
    pub col: usize,
}

pub struct FormedWord {
    pub word: String,
    /// (letter, row, col, is_new) for every letter in the word.
    pub cells: Vec<(char, usize, usize, bool)>,
}

/// All words formed by placing `placed` onto `board` (board does NOT yet
/// contain the new tiles). Returns Err(reason) for illegal geometry.
pub fn formed_words(board: &Board, placed: &[Placed]) -> Result<Vec<FormedWord>, String> {
    if placed.is_empty() {
        return Err("No tiles placed".into());
    }
    for p in placed {
        if p.row >= BOARD_SIZE || p.col >= BOARD_SIZE {
            return Err("Tile out of bounds".into());
        }
        if !board[p.row][p.col].is_empty() {
            return Err("Cell is already occupied".into());
        }
    }
    let same_row = placed.iter().all(|p| p.row == placed[0].row);
    let same_col = placed.iter().all(|p| p.col == placed[0].col);
    if !same_row && !same_col {
        return Err("Tiles must be in one row or one column".into());
    }
    if placed.len() > 1 {
        // no gaps among placed tiles along the line
        if same_row {
            let cols: Vec<usize> = placed.iter().map(|p| p.col).collect();
            for (c, cell) in board[placed[0].row].iter().enumerate().take(cols.iter().max().unwrap() + 1).skip(*cols.iter().min().unwrap()) {
                if !cols.contains(&c) && cell.is_empty() {
                    return Err("Tiles must be contiguous (no gaps)".into());
                }
            }
        } else {
            let rows: Vec<usize> = placed.iter().map(|p| p.row).collect();
            for (r, row) in board.iter().enumerate().take(rows.iter().max().unwrap() + 1).skip(*rows.iter().min().unwrap()) {
                if !rows.contains(&r) && row[placed[0].col].is_empty() {
                    return Err("Tiles must be contiguous (no gaps)".into());
                }
            }
        }
    }

    let is_new = |r: usize, c: usize| placed.iter().any(|p| p.row == r && p.col == c);
    let letter_at = |r: usize, c: usize| -> Option<char> {
        if let Some(p) = placed.iter().find(|p| p.row == r && p.col == c) {
            return Some(p.letter);
        }
        board[r][c].chars().next().filter(|_| !board[r][c].is_empty())
    };

    let mut words: Vec<FormedWord> = Vec::new();
    let mut seen: Vec<(usize, usize, usize, usize)> = Vec::new(); // start,end of word span

    let mut collect = |dr: i64, dc: i64, r0: usize, c0: usize| {
        // walk backwards to word start
        let mut r = r0 as i64;
        let mut c = c0 as i64;
        loop {
            let pr = r - dr;
            let pc = c - dc;
            if pr < 0 || pc < 0 || pr as usize >= BOARD_SIZE || pc as usize >= BOARD_SIZE {
                break;
            }
            if letter_at(pr as usize, pc as usize).is_none() {
                break;
            }
            r = pr;
            c = pc;
        }
        let start = (r, c);
        let mut cells: Vec<(char, usize, usize, bool)> = Vec::new();
        while r >= 0 && c >= 0 && (r as usize) < BOARD_SIZE && (c as usize) < BOARD_SIZE {
            match letter_at(r as usize, c as usize) {
                Some(ch) => cells.push((ch, r as usize, c as usize, is_new(r as usize, c as usize))),
                None => break,
            }
            r += dr;
            c += dc;
        }
        if cells.len() >= 2 {
            let span = (
                start.0 as usize,
                start.1 as usize,
                cells.last().unwrap().1,
                cells.last().unwrap().2,
            );
            if !seen.contains(&span) {
                seen.push(span);
                let word: String = cells.iter().map(|(ch, _, _, _)| ch).collect();
                words.push(FormedWord { word, cells });
            }
        }
    };

    if same_row {
        // main word along the row
        collect(0, 1, placed[0].row, placed[0].col);
        // cross words along columns
        for p in placed {
            collect(1, 0, p.row, p.col);
        }
    } else {
        collect(1, 0, placed[0].row, placed[0].col);
        for p in placed {
            collect(0, 1, p.row, p.col);
        }
    }

    if words.is_empty() {
        return Err("Move must form a word of at least two letters".into());
    }
    Ok(words)
}

fn board_is_empty(board: &Board) -> bool {
    board.iter().all(|row| row.iter().all(|c| c.is_empty()))
}

pub fn score_words(words: &[FormedWord]) -> i64 {
    let mut total = 0;
    for w in words {
        let mut word_mult = 1;
        let mut sum = 0;
        for (ch, r, c, is_new) in &w.cells {
            let mut pts = letter_points(*ch);
            if *is_new {
                let (lm, wm) = bonus_at(*r, *c);
                pts *= lm;
                word_mult *= wm;
            }
            sum += pts;
        }
        total += sum * word_mult;
    }
    total
}

pub mod test {
    pub use super::{
        dictionary, formed_words, fresh_bag, score_words, Board, Placed, BOARD_SIZE,
    };
}

// ---------------------------------------------------------------------------
// Plugin / routes

pub struct WordgamePlugin;

impl WordgamePlugin {
    pub const MANIFEST: syarc_plugin_api::AppManifest =
        syarc_plugin_api::AppManifest::new("wordgame", "Word Game", "/wordgame")
            .icon("bi-grid-3x3-gap");
}

#[async_trait::async_trait]
impl syarc_plugin_api::AppPlugin for WordgamePlugin {
    fn manifest(&self) -> &'static syarc_plugin_api::AppManifest {
        &Self::MANIFEST
    }

    fn router(&self, state: SharedState) -> Router<SharedState> {
        Router::new()
            .route("/wordgame", get(home_page))
            .route("/wordgame/", get(home_page))
            .route("/wordgame/game/{game_id}", get(game_page))
            .route("/wordgame/api/games", get(list_games).post(create_game))
            .route("/wordgame/api/games/{game_id}", get(get_game))
            .route("/wordgame/api/games/{game_id}/join", post(join_game))
            .route("/wordgame/api/games/{game_id}/move", post(play_move))
            .route("/wordgame/api/games/{game_id}/pass", post(pass_turn))
            .route("/wordgame/api/stats", get(stats))
            .with_state(state)
    }

    async fn on_startup(&self, db: &syarc_plugin_api::Db) -> Result<(), sqlx::Error> {
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS word_games (\
             id TEXT PRIMARY KEY, player1_id INTEGER NOT NULL, player2_id INTEGER,\
             current_turn INTEGER NOT NULL, player1_score INTEGER DEFAULT 0,\
             player2_score INTEGER DEFAULT 0, player1_tiles TEXT DEFAULT '[]',\
             player2_tiles TEXT DEFAULT '[]', tile_bag TEXT DEFAULT '[]',\
             consecutive_passes INTEGER DEFAULT 0,\
             status TEXT DEFAULT 'waiting', winner_id INTEGER,\
             created_at TEXT, updated_at TEXT, last_move_at TEXT)",
        )
        .execute(db)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS word_boards (\
             game_id TEXT PRIMARY KEY REFERENCES word_games(id),\
             board TEXT NOT NULL, updated_at TEXT)",
        )
        .execute(db)
        .await?;
        sqlx::query(
            "CREATE TABLE IF NOT EXISTS word_moves (\
             id TEXT PRIMARY KEY, game_id TEXT NOT NULL REFERENCES word_games(id),\
             player_id INTEGER NOT NULL, move_number INTEGER NOT NULL,\
             word TEXT NOT NULL, score INTEGER DEFAULT 0,\
             tiles_placed TEXT DEFAULT '[]', is_bonus INTEGER DEFAULT 0,\
             created_at TEXT)",
        )
        .execute(db)
        .await?;
        // Grant to every active human account (mirrors hangman seeding).
        sqlx::query(
            "INSERT OR IGNORE INTO user_app_permissions (user_id, app_id) \
             SELECT id, 'wordgame' FROM users WHERE is_active = 1 AND username NOT LIKE 'mesh-%'",
        )
        .execute(db)
        .await?;
        Ok(())
    }
}

// --- helpers -------------------------------------------------------------

fn err_json(status: axum::http::StatusCode, msg: &str) -> Response {
    (status, axum::Json(json!({"error": msg}))).into_response()
}
fn err400(msg: &str) -> Response {
    err_json(axum::http::StatusCode::BAD_REQUEST, msg)
}
fn err403(msg: &str) -> Response {
    err_json(axum::http::StatusCode::FORBIDDEN, msg)
}
fn err404(msg: &str) -> Response {
    err_json(axum::http::StatusCode::NOT_FOUND, msg)
}
fn ok_json(v: Value) -> Response {
    axum::Json(v).into_response()
}

struct GameRow {
    id: String,
    player1_id: i64,
    player2_id: Option<i64>,
    current_turn: i64,
    player1_score: i64,
    player2_score: i64,
    player1_tiles: String,
    player2_tiles: String,
    tile_bag: String,
    consecutive_passes: i64,
    status: String,
    winner_id: Option<i64>,
    updated_at: String,
}

fn game_row(r: &sqlx::sqlite::SqliteRow) -> GameRow {
    GameRow {
        id: r.get("id"),
        player1_id: r.get("player1_id"),
        player2_id: r.try_get::<Option<i64>, _>("player2_id").ok().flatten(),
        current_turn: r.get("current_turn"),
        player1_score: r.get("player1_score"),
        player2_score: r.get("player2_score"),
        player1_tiles: r.get("player1_tiles"),
        player2_tiles: r.get("player2_tiles"),
        tile_bag: r.get("tile_bag"),
        consecutive_passes: r.get("consecutive_passes"),
        status: r.get("status"),
        winner_id: r.try_get::<Option<i64>, _>("winner_id").ok().flatten(),
        updated_at: r.get("updated_at"),
    }
}

async fn load_game(state: &SharedState, id: &str) -> Option<GameRow> {
    sqlx::query("SELECT * FROM word_games WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()?
        .map(|r| game_row(&r))
}

async fn load_board(state: &SharedState, game_id: &str) -> Board {
    let row: Option<String> = sqlx::query_scalar("SELECT board FROM word_boards WHERE game_id = ?")
        .bind(game_id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten();
    row.map(|s| parse_board(&s)).unwrap_or_else(empty_board)
}

async fn username_of(state: &SharedState, id: i64) -> Option<String> {
    sqlx::query_scalar::<_, String>("SELECT username FROM users WHERE id = ?")
        .bind(id)
        .fetch_optional(&state.db)
        .await
        .ok()
        .flatten()
}

fn draw(bag: &mut Vec<char>, count: usize) -> Vec<char> {
    let mut drawn = Vec::new();
    while drawn.len() < count && !bag.is_empty() {
        drawn.push(bag.remove(0));
    }
    drawn
}

/// Full state payload for the game page (tiles hidden from opponent).
async fn game_payload(state: &SharedState, g: &GameRow, user_id: i64) -> Value {
    let board = load_board(state, &g.id).await;
    let p1 = username_of(state, g.player1_id).await;
    let p2 = match g.player2_id {
        Some(id) => username_of(state, id).await,
        None => None,
    };
    let i_am_p1 = g.player1_id == user_id;
    let my_tiles = parse_tiles(if i_am_p1 { &g.player1_tiles } else { &g.player2_tiles });
    let bag: Vec<String> =
        serde_json::from_str(&g.tile_bag).unwrap_or_default();
    let winner_username = match g.winner_id {
        Some(id) => username_of(state, id).await,
        None => None,
    };
    json!({
        "id": g.id,
        "status": g.status,
        "player1": {"id": g.player1_id, "username": p1, "score": g.player1_score},
        "player2": {"id": g.player2_id, "username": p2, "score": g.player2_score},
        "current_turn": g.current_turn,
        "current_turn_username":
            if g.current_turn == g.player1_id { p1.clone() } else { p2.clone() },
        "winner_id": g.winner_id,
        "winner_username": winner_username,
        "is_my_turn": g.current_turn == user_id && g.status == "active",
        "i_am_player1": i_am_p1,
        "my_tiles": my_tiles,
        "tile_bag_remaining": bag.len(),
        "board": board,
        "consecutive_passes": g.consecutive_passes,
    })
}

// --- pages ----------------------------------------------------------------

async fn home_page(State(state): State<SharedState>, RequireUser(_u): RequireUser) -> Response {
    Html(state.templates.render("wordgame.html", json!({}))).into_response()
}

async fn game_page(
    State(state): State<SharedState>,
    RequireUser(_u): RequireUser,
    AxPath(_game_id): AxPath<String>,
) -> Response {
    Html(state.templates.render("wordgame_game.html", json!({}))).into_response()
}

// --- api ------------------------------------------------------------------

async fn list_games(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    axum::extract::Query(q): axum::extract::Query<std::collections::HashMap<String, String>>,
) -> Response {
    let status = q.get("status").cloned().unwrap_or_else(|| "all".into());
    let rows = sqlx::query(
        "SELECT * FROM word_games WHERE player1_id = ? OR player2_id = ? \
         ORDER BY updated_at DESC LIMIT 100",
    )
    .bind(user.id)
    .bind(user.id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let mut games: Vec<Value> = Vec::new();
    for g in rows.iter().map(game_row) {
        if status != "all" && g.status != status {
            continue;
        }
        let i_am_p1 = g.player1_id == user.id;
        let opp_id = if i_am_p1 { g.player2_id } else { Some(g.player1_id) };
        let opp = match opp_id {
            Some(id) => username_of(&state, id).await,
            None => None,
        };
        let (my_score, opp_score) = if i_am_p1 {
            (g.player1_score, g.player2_score)
        } else {
            (g.player2_score, g.player1_score)
        };
        games.push(json!({
            "id": g.id,
            "opponent_username": opp,
            "status": g.status,
            "my_score": my_score,
            "opponent_score": opp_score,
            "is_my_turn": g.current_turn == user.id && g.status == "active",
            "updated_at": g.updated_at,
        }));
    }
    ok_json(json!({"games": games}))
}

#[derive(Deserialize)]
struct CreateGame {
    opponent_username: String,
}

async fn create_game(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    axum::Json(body): axum::Json<CreateGame>,
) -> Response {
    let opponent = sqlx::query_as::<_, (i64,)>(
        "SELECT id FROM users WHERE username = ? AND is_active = 1",
    )
    .bind(body.opponent_username.trim())
    .fetch_optional(&state.db)
    .await;
    let Ok(opp_opt) = opponent else { return err400("Lookup failed"); };
    let Some((opp_id,)) = opp_opt else {
        return err404("Opponent not found");
    };
    if opp_id == user.id {
        return err400("You cannot play against yourself");
    }
    let mut bag = fresh_bag();
    let p1_tiles: Vec<String> = draw(&mut bag, RACK_SIZE).iter().map(|c| c.to_string()).collect();
    let p2_tiles: Vec<String> = draw(&mut bag, RACK_SIZE).iter().map(|c| c.to_string()).collect();
    let id = new_uuid();
    let now = now_str();
    let res = sqlx::query(
        "INSERT INTO word_games (id, player1_id, player2_id, current_turn, player1_tiles, \
         player2_tiles, tile_bag, status, created_at, updated_at) \
         VALUES (?, ?, ?, ?, ?, ?, ?, 'active', ?, ?)",
    )
    .bind(&id)
    .bind(user.id)
    .bind(opp_id)
    .bind(user.id)
    .bind(serde_json::to_string(&p1_tiles).unwrap())
    .bind(serde_json::to_string(&p2_tiles).unwrap())
    .bind(serde_json::to_string(&bag_as_strings(&bag)).unwrap())
    .bind(&now)
    .bind(&now)
    .execute(&state.db)
    .await;
    if res.is_err() {
        return err400("Could not create game");
    }
    if sqlx::query("INSERT INTO word_boards (game_id, board, updated_at) VALUES (?, ?, ?)")
        .bind(&id)
        .bind(serde_json::to_string(&empty_board()).unwrap())
        .bind(&now)
        .execute(&state.db)
        .await
        .is_err()
    {
        return err400("Could not create board");
    }
    ok_json(json!({"message": "Game created", "game_id": id}))
}

fn bag_as_strings(bag: &[char]) -> Vec<String> {
    bag.iter().map(|c| c.to_string()).collect()
}

async fn join_game(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(game_id): AxPath<String>,
) -> Response {
    let Some(mut g) = load_game(&state, &game_id).await else {
        return err404("Game not found");
    };
    if g.player2_id != Some(user.id) {
        return err403("You were not invited to this game");
    }
    if g.status != "waiting" {
        return ok_json(json!({"message": "Already joined", "status": g.status}));
    }
    let mut bag: Vec<char> = parse_tiles(&g.tile_bag);
    let p2: Vec<String> = draw(&mut bag, RACK_SIZE).iter().map(|c| c.to_string()).collect();
    let _ = sqlx::query(
        "UPDATE word_games SET status = 'active', player2_tiles = ?, tile_bag = ?, \
         current_turn = player1_id, updated_at = ? WHERE id = ?",
    )
    .bind(serde_json::to_string(&p2).unwrap())
    .bind(serde_json::to_string(&bag_as_strings(&bag)).unwrap())
    .bind(now_str())
    .bind(&game_id)
    .execute(&state.db)
    .await;
    g.status = "active".into();
    ok_json(json!({"message": "Joined", "status": "active"}))
}

async fn get_game(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(game_id): AxPath<String>,
) -> Response {
    let Some(g) = load_game(&state, &game_id).await else {
        return err404("Game not found");
    };
    if g.player1_id != user.id && g.player2_id != Some(user.id) {
        return err403("Not a player in this game");
    }
    let mut payload = game_payload(&state, &g, user.id).await;
    if g.status != "completed" {
        // hide opponent tiles
        payload["opponent_tiles_hidden"] = json!(true);
    }
    // move history
    let moves = sqlx::query(
        "SELECT * FROM word_moves WHERE game_id = ? ORDER BY move_number",
    )
    .bind(&game_id)
    .fetch_all(&state.db)
    .await
    .unwrap_or_default();
    let mut history: Vec<Value> = Vec::new();
    for m in &moves {
        let player_id: i64 = m.get("player_id");
        let player_username = username_of(&state, player_id).await;
        history.push(json!({
            "player_id": player_id,
            "player_username": player_username,
            "move_number": m.get::<i64, _>("move_number"),
            "word": m.get::<String, _>("word"),
            "score": m.get::<i64, _>("score"),
            "is_bonus": m.get::<i64, _>("is_bonus") == 1,
        }));
    }
    payload["moves"] = json!(history);
    ok_json(payload)
}

#[derive(Deserialize)]
struct MoveBody {
    tiles: Vec<TilePlacement>,
}

async fn play_move(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(game_id): AxPath<String>,
    axum::Json(body): axum::Json<MoveBody>,
) -> Response {
    let Some(g) = load_game(&state, &game_id).await else {
        return err404("Game not found");
    };
    if g.player1_id != user.id && g.player2_id != Some(user.id) {
        return err403("Not a player in this game");
    }
    if g.status != "active" {
        return err400("Game is not active");
    }
    if g.current_turn != user.id {
        return err400("It is not your turn");
    }
    if body.tiles.is_empty() || body.tiles.len() > RACK_SIZE {
        return err400("Place between 1 and 7 tiles");
    }

    let i_am_p1 = g.player1_id == user.id;
    let mut rack = parse_tiles(if i_am_p1 { &g.player1_tiles } else { &g.player2_tiles });
    let board = load_board(&state, &game_id).await;

    // placements
    let mut placed: Vec<Placed> = Vec::new();
    for t in &body.tiles {
        let Some(letter) = t.letter.trim().to_uppercase().chars().next() else {
            return err400("Invalid tile letter");
        };
        if !letter.is_ascii_alphabetic() {
            return err400("Invalid tile letter");
        }
        placed.push(Placed { letter, row: t.row, col: t.col });
    }
    // duplicates of cells
    let mut seen_cells = std::collections::HashSet::new();
    for p in &placed {
        if !seen_cells.insert((p.row, p.col)) {
            return err400("Two tiles on the same cell");
        }
    }
    // letters must be available in rack
    let mut rack_check = rack.clone();
    for p in &placed {
        match rack_check.iter().position(|c| *c == p.letter) {
            Some(i) => {
                rack_check.remove(i);
            }
            None => return err400("Letter not in your rack"),
        }
    }

    // words + geometry
    let words = match formed_words(&board, &placed) {
        Ok(w) => w,
        Err(e) => return err400(&e),
    };
    // dictionary
    for w in &words {
        if !dictionary().contains(&w.word) {
            return err400(&format!("'{}' is not a valid word", w.word));
        }
    }
    // placement rules: first move covers center; later moves connect
    if board_is_empty(&board) {
        if !placed.iter().any(|p| p.row == 7 && p.col == 7) {
            return err400("The first word must cover the center square");
        }
        if words.iter().all(|w| w.word.len() < 2) {
            return err400("The first word must be at least two letters");
        }
    } else {
        let touches = placed.iter().any(|p| {
            let (r, c) = (p.row as i64, p.col as i64);
            [(r - 1, c), (r + 1, c), (r, c - 1), (r, c + 1)].iter().any(|(rr, cc)| {
                *rr >= 0
                    && *cc >= 0
                    && (*rr as usize) < BOARD_SIZE
                    && (*cc as usize) < BOARD_SIZE
                    && !board[*rr as usize][*cc as usize].is_empty()
            })
        });
        if !touches {
            return err400("New words must connect to existing tiles");
        }
    }

    let mut score = score_words(&words);
    let is_bonus = placed.len() == RACK_SIZE;
    if is_bonus {
        score += 50;
    }

    // commit: remove used letters from rack
    for p in &placed {
        if let Some(i) = rack.iter().position(|c| *c == p.letter) {
            rack.remove(i);
        }
    }
    let mut bag: Vec<char> = parse_tiles(&g.tile_bag);
    let drawn = draw(&mut bag, RACK_SIZE - rack.len());
    rack.extend(drawn);

    // update board
    let mut new_board = board.clone();
    for p in &placed {
        new_board[p.row][p.col] = p.letter.to_string();
    }

    let main_word = words
        .iter()
        .max_by_key(|w| w.word.len())
        .map(|w| w.word.clone())
        .unwrap_or_default();
    let move_number: i64 = sqlx::query_scalar::<_, i64>(
        "SELECT COALESCE(MAX(move_number), 0) + 1 FROM word_moves WHERE game_id = ?",
    )
    .bind(&game_id)
    .fetch_one(&state.db)
    .await
    .unwrap_or(1);

    let _ = sqlx::query("UPDATE word_boards SET board = ?, updated_at = ? WHERE game_id = ?")
        .bind(serde_json::to_string(&new_board).unwrap())
        .bind(now_str())
        .bind(&game_id)
        .execute(&state.db)
        .await;

    let tiles_json = serde_json::to_string(
        &placed
            .iter()
            .map(|p| json!({"letter": p.letter.to_string(), "row": p.row, "col": p.col}))
            .collect::<Vec<_>>(),
    )
    .unwrap();
    let _ = sqlx::query(
        "INSERT INTO word_moves (id, game_id, player_id, move_number, word, score, \
         tiles_placed, is_bonus, created_at) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new_uuid())
    .bind(&game_id)
    .bind(user.id)
    .bind(move_number)
    .bind(&main_word)
    .bind(score)
    .bind(&tiles_json)
    .bind(if is_bonus { 1 } else { 0 })
    .bind(now_str())
    .execute(&state.db)
    .await;

    // game end: bag empty and either rack empty
    let game_over = bag.is_empty() && rack.is_empty();
    let (p1_score, p2_score) = if i_am_p1 {
        (g.player1_score + score, g.player2_score)
    } else {
        (g.player1_score, g.player2_score + score)
    };
    let (status, winner_id) = if game_over {
        let winner = if p1_score > p2_score {
            Some(g.player1_id)
        } else if p2_score > p1_score {
            g.player2_id
        } else {
            None
        };
        ("completed", winner)
    } else {
        (g.status.as_str(), g.winner_id)
    };
    let next_turn = if i_am_p1 { g.player2_id } else { Some(g.player1_id) };

    let _ = sqlx::query(
        "UPDATE word_games SET player1_score = ?, player2_score = ?, \
         player1_tiles = ?, player2_tiles = ?, tile_bag = ?, current_turn = ?, \
         consecutive_passes = 0, status = ?, winner_id = ?, last_move_at = ?, updated_at = ? \
         WHERE id = ?",
    )
    .bind(p1_score)
    .bind(p2_score)
    .bind(if i_am_p1 {
        serde_json::to_string(&rack.iter().map(|c| c.to_string()).collect::<Vec<_>>()).unwrap()
    } else {
        g.player1_tiles.clone()
    })
    .bind(if i_am_p1 {
        g.player2_tiles.clone()
    } else {
        serde_json::to_string(&rack.iter().map(|c| c.to_string()).collect::<Vec<_>>()).unwrap()
    })
    .bind(serde_json::to_string(&bag_as_strings(&bag)).unwrap())
    .bind(next_turn.unwrap_or(g.player1_id))
    .bind(status)
    .bind(winner_id)
    .bind(now_str())
    .bind(now_str())
    .bind(&game_id)
    .execute(&state.db)
    .await;

    ok_json(json!({
        "message": "Move played",
        "word": main_word,
        "score": score,
        "bonus": is_bonus,
        "game_over": game_over,
    }))
}

async fn pass_turn(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
    AxPath(game_id): AxPath<String>,
) -> Response {
    let Some(g) = load_game(&state, &game_id).await else {
        return err404("Game not found");
    };
    if g.player1_id != user.id && g.player2_id != Some(user.id) {
        return err403("Not a player in this game");
    }
    if g.status != "active" {
        return err400("Game is not active");
    }
    if g.current_turn != user.id {
        return err400("It is not your turn");
    }
    let passes = g.consecutive_passes + 1;
    let next_turn = if g.player1_id == user.id { g.player2_id } else { Some(g.player1_id) };
    if passes >= 3 {
        // three consecutive passes end the game; higher score wins
        let winner = if g.player1_score > g.player2_score {
            Some(g.player1_id)
        } else if g.player2_score > g.player1_score {
            g.player2_id
        } else {
            None
        };
        let _ = sqlx::query(
            "UPDATE word_games SET status = 'completed', winner_id = ?, current_turn = ?, \
             consecutive_passes = ?, last_move_at = ?, updated_at = ? WHERE id = ?",
        )
        .bind(winner)
        .bind(next_turn.unwrap_or(g.player1_id))
        .bind(passes)
        .bind(now_str())
        .bind(now_str())
        .bind(&game_id)
        .execute(&state.db)
        .await;
        return ok_json(json!({"message": "Game ended by consecutive passes"}));
    }
    let _ = sqlx::query(
        "UPDATE word_games SET current_turn = ?, consecutive_passes = ?, \
         last_move_at = ?, updated_at = ? WHERE id = ?",
    )
    .bind(next_turn.unwrap_or(g.player1_id))
    .bind(passes)
    .bind(now_str())
    .bind(now_str())
    .bind(&game_id)
    .execute(&state.db)
    .await;
    ok_json(json!({"message": "Turn passed"}))
}

async fn stats(
    State(state): State<SharedState>,
    RequireUser(user): RequireUser,
) -> Response {
    let rows = sqlx::query("SELECT * FROM word_games WHERE player1_id = ? OR player2_id = ?")
        .bind(user.id)
        .bind(user.id)
        .fetch_all(&state.db)
        .await
        .unwrap_or_default();
    let mut total = 0i64;
    let mut won = 0i64;
    let mut lost = 0i64;
    let mut active = 0i64;
    let mut my_points = 0i64;
    for r in &rows {
        let g = game_row(r);
        total += 1;
        let (mine, _theirs) = if g.player1_id == user.id {
            (g.player1_score, g.player2_score)
        } else {
            (g.player2_score, g.player1_score)
        };
        my_points += mine;
        match g.status.as_str() {
            "completed" => {
                if g.winner_id == Some(user.id) {
                    won += 1;
                } else if g.winner_id.is_some() {
                    lost += 1;
                }
            }
            "active" => active += 1,
            _ => {}
        }
    }
    ok_json(json!({
        "total_games": total,
        "games_won": won,
        "games_lost": lost,
        "games_active": active,
        "total_score": my_points,
        "average_score": if total > 0 { (my_points as f64 / total as f64 * 10.0).round() / 10.0 } else { 0.0 },
    }))
}
