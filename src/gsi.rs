//! Dota 2 Game State Integration (official Valve feature).
//!
//! Dota reads `game/dota/cfg/gamestate_integration/*.cfg` when started with
//! `-gamestateintegration` and POSTs JSON describing the game state to the URI
//! from that file. We only use it to tell "in a match" from "in the menu", so the
//! launcher never closes the game mid-match.
//!
//! The first process (launcher or companion) to bind the port owns the server;
//! the other one asks it via `GET /state`.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpListener, TcpStream};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Mutex;
use std::time::{Duration, Instant};

pub const PORT: u16 = 53742;
pub const LAUNCH_FLAG: &str = "-gamestateintegration";
/// Payloads older than this are treated as "unknown" (heartbeat is 10 s).
const STALE: Duration = Duration::from_secs(30);

static OWN_SERVER: AtomicBool = AtomicBool::new(false);
static LAST: Mutex<Option<(Instant, serde_json::Value)>> = Mutex::new(None);

#[derive(Clone, Debug, PartialEq)]
pub enum GameState {
    /// Dota is in the main menu / not in a match.
    Menu,
    /// Match finished, scoreboard shown — safe to close.
    PostGame,
    /// Any in-match state (hero pick, strategy, game in progress...).
    InMatch(String),
    /// No fresh data (Dota started without -gamestateintegration, or not running).
    Unknown,
}

impl GameState {
    pub fn safe_to_close(&self) -> bool {
        matches!(self, GameState::Menu | GameState::PostGame)
    }

    pub fn label(&self) -> &'static str {
        match self {
            GameState::Menu => crate::tr!("в меню", "in the menu"),
            GameState::PostGame => crate::tr!("матч окончен", "match over"),
            GameState::InMatch(_) => crate::tr!("идёт матч", "match on"),
            GameState::Unknown => crate::tr!("состояние неизвестно", "state unknown"),
        }
    }
}

const CFG_NAME: &str = "gamestate_integration_dotaskins.cfg";

fn cfg_text() -> String {
    format!(
        "\"Dota Skins Launcher\"\n{{\n\t\"uri\"\t\t\"http://127.0.0.1:{PORT}/\"\n\t\"timeout\"\t\"5.0\"\n\t\"buffer\"\t\"0.5\"\n\t\"throttle\"\t\"1.0\"\n\t\"heartbeat\"\t\"10.0\"\n\t\"data\"\n\t{{\n\t\t\"provider\"\t\"1\"\n\t\t\"map\"\t\t\"1\"\n\t}}\n}}\n"
    )
}

/// Writes the GSI config into `<Dota>/game/dota/cfg/gamestate_integration/`.
/// Returns true when the file was created or updated.
pub fn install_cfg(game_dir: &Path) -> Result<bool, String> {
    let dir = game_dir.join("cfg").join("gamestate_integration");
    let file = dir.join(CFG_NAME);
    let text = cfg_text();
    if std::fs::read_to_string(&file).ok().as_deref() == Some(text.as_str()) {
        return Ok(false);
    }
    std::fs::create_dir_all(&dir).map_err(|e| format!("cannot create {}: {e}", dir.display()))?;
    std::fs::write(&file, text).map_err(|e| format!("cannot write {}: {e}", file.display()))?;
    Ok(true)
}

/// Adds `-gamestateintegration` to launch arguments when missing.
pub fn with_launch_flag(args: &str) -> String {
    if args.split_whitespace().any(|a| a.eq_ignore_ascii_case(LAUNCH_FLAG)) {
        args.to_string()
    } else if args.trim().is_empty() {
        LAUNCH_FLAG.to_string()
    } else {
        format!("{} {LAUNCH_FLAG}", args.trim())
    }
}

/// Starts the local listener if the port is free. Safe to call more than once.
pub fn start_server() {
    if OWN_SERVER.load(Ordering::SeqCst) {
        return;
    }
    let Ok(listener) = TcpListener::bind(("127.0.0.1", PORT)) else {
        return; // another instance owns it
    };
    OWN_SERVER.store(true, Ordering::SeqCst);
    std::thread::Builder::new()
        .name("gsi-server".into())
        .spawn(move || {
            for stream in listener.incoming().flatten() {
                let _ = handle(stream);
            }
        })
        .ok();
}

fn handle(mut stream: TcpStream) -> std::io::Result<()> {
    stream.set_read_timeout(Some(Duration::from_secs(3)))?;
    let (head, body) = read_http(&mut stream)?;
    let first = head.lines().next().unwrap_or("");

    if first.starts_with("GET /state") {
        let snapshot = LAST.lock().ok().and_then(|g| g.clone());
        let json = match snapshot {
            Some((at, payload)) => serde_json::json!({
                "age_ms": at.elapsed().as_millis() as u64,
                "payload": payload,
            }),
            None => serde_json::json!({ "age_ms": null, "payload": null }),
        };
        respond(&mut stream, &json.to_string())
    } else if first.starts_with("POST") {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&body) {
            if let Ok(mut g) = LAST.lock() {
                *g = Some((Instant::now(), value));
            }
        }
        respond(&mut stream, "")
    } else {
        respond(&mut stream, "")
    }
}

fn respond(stream: &mut TcpStream, body: &str) -> std::io::Result<()> {
    write!(
        stream,
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    )
}

/// Reads an HTTP message: returns (header text, body bytes).
fn read_http(stream: &mut TcpStream) -> std::io::Result<(String, Vec<u8>)> {
    let mut buf = Vec::with_capacity(8192);
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break buf.len();
        }
        buf.extend_from_slice(&chunk[..n]);
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        if buf.len() > 1 << 20 {
            break buf.len();
        }
    };
    let head = String::from_utf8_lossy(&buf[..header_end.min(buf.len())]).into_owned();
    let length = head
        .lines()
        .find_map(|l| {
            let (k, v) = l.split_once(':')?;
            k.trim()
                .eq_ignore_ascii_case("content-length")
                .then(|| v.trim().parse::<usize>().ok())
                .flatten()
        })
        .unwrap_or(0)
        .min(4 << 20);
    let mut body = buf[header_end.min(buf.len())..].to_vec();
    while body.len() < length {
        let n = stream.read(&mut chunk)?;
        if n == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    body.truncate(length);
    Ok((head, body))
}

/// Current game state, from our own server or from the instance that owns it.
pub fn query() -> GameState {
    let (age, payload) = if OWN_SERVER.load(Ordering::SeqCst) {
        match LAST.lock().ok().and_then(|g| g.clone()) {
            Some((at, p)) => (Some(at.elapsed()), Some(p)),
            None => (None, None),
        }
    } else {
        ask_owner().unwrap_or((None, None))
    };
    match (age, payload) {
        (Some(age), Some(payload)) if age <= STALE => classify(&payload),
        _ => GameState::Unknown,
    }
}

fn ask_owner() -> Option<(Option<Duration>, Option<serde_json::Value>)> {
    let addr = SocketAddr::from(([127, 0, 0, 1], PORT));
    let mut stream = TcpStream::connect_timeout(&addr, Duration::from_millis(300)).ok()?;
    stream.set_read_timeout(Some(Duration::from_secs(1))).ok()?;
    stream
        .write_all(b"GET /state HTTP/1.1\r\nHost: 127.0.0.1\r\nConnection: close\r\n\r\n")
        .ok()?;
    let (_, body) = read_http(&mut stream).ok()?;
    let json: serde_json::Value = serde_json::from_slice(&body).ok()?;
    let age = json
        .get("age_ms")
        .and_then(|v| v.as_u64())
        .map(Duration::from_millis);
    let payload = json.get("payload").filter(|p| !p.is_null()).cloned();
    Some((age, payload))
}

fn classify(payload: &serde_json::Value) -> GameState {
    match payload.get("map").and_then(|m| m.get("game_state")).and_then(|s| s.as_str()) {
        None => GameState::Menu,
        Some("DOTA_GAMERULES_STATE_POST_GAME") => GameState::PostGame,
        Some(state) => GameState::InMatch(state.to_string()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classifies_payloads() {
        let menu = serde_json::json!({ "provider": { "name": "Dota 2" } });
        assert_eq!(classify(&menu), GameState::Menu);
        let live = serde_json::json!({ "map": { "game_state": "DOTA_GAMERULES_STATE_GAME_IN_PROGRESS" } });
        assert!(matches!(classify(&live), GameState::InMatch(_)));
        assert!(!classify(&live).safe_to_close());
        let post = serde_json::json!({ "map": { "game_state": "DOTA_GAMERULES_STATE_POST_GAME" } });
        assert!(classify(&post).safe_to_close());
    }

    #[test]
    fn launch_flag_added_once() {
        assert_eq!(with_launch_flag("-novid"), "-novid -gamestateintegration");
        assert_eq!(with_launch_flag("-novid -gamestateintegration"), "-novid -gamestateintegration");
        assert_eq!(with_launch_flag(""), "-gamestateintegration");
    }
}
