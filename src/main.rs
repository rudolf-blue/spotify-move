use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use base64::{Engine, engine::general_purpose::STANDARD as BASE64};
use chromiumoxide::{Browser, BrowserConfig, Page};
use clap::{Parser, Subcommand};
use futures::StreamExt;
use regex::Regex;
use reqwest::{Method, StatusCode};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::time::sleep;

const PATHFINDER: &str = "https://api-partner.spotify.com/pathfinder/v2/query";
const PLAYLIST_API: &str = "https://spclient.wg.spotify.com/playlist/v2";
const DECORATE: &str = "decorate=revision,length,attributes,timestamp,owner";
const WEB_PLAYER: &str = "https://open.spotify.com";
const IMAGE_UPLOAD: &str = "https://image-upload.spotify.com/v4/playlist";
const LOGIN_TIMEOUT: Duration = Duration::from_secs(60 * 60);
const TOKEN_MAX_AGE: Duration = Duration::from_secs(40 * 60);
const PAGE_STALL: Duration = Duration::from_secs(20);
//saved dates only have one-second resolution
const SAVE_SPACING: Duration = Duration::from_millis(1100);
const LIBRARY_KINDS: &[&str] = &["album", "artist", "show", "episode", "audiobook"];

#[derive(Parser)]
#[command(about = "Copy a Spotify library from one account to another")]
struct Cli {
    ///Directory with a browser profile for each account
    #[arg(long, default_value = "profiles")]
    profiles: PathBuf,
    ///Chrome, Chromium or Edge binary to use
    #[arg(long)]
    chrome: Option<PathBuf>,
    #[command(subcommand)]
    cmd: Cmd,
}

#[derive(Subcommand)]
enum Cmd {
    ///Save an account's library to a JSON file (read-only)
    Export {
        ///Profile name, e.g. "old"
        profile: String,
        #[arg(short, long)]
        out: Option<PathBuf>,
    },
    ///Copy a saved library into this account (resumable)
    Import {
        profile: String,
        backup: PathBuf,
        ///Save in batches
        #[arg(long)]
        fast: bool,
    },
    ///Compare an account against a saved library
    Verify { profile: String, backup: PathBuf },
}

#[derive(Serialize, Deserialize)]
struct Backup {
    user: String,
    ///Newest first
    liked: Vec<Saved>,
    library: Vec<Saved>,
    ///In sidebar order
    playlists: Vec<Entry>,
    pinned: Vec<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Saved {
    uri: String,
    name: Option<String>,
    added_at: Option<String>,
}

///A playlist or a folder marker
#[derive(Serialize, Deserialize, Clone)]
struct Entry {
    uri: String,
    name: Option<String>,
    description: Option<String>,
    owner: Option<String>,
    public: Option<bool>,
    #[serde(default)]
    cover: Option<String>,
    items: Vec<Item>,
}

#[derive(Serialize, Deserialize, Clone)]
struct Item {
    uri: String,
    added_at: Option<String>,
}

impl Entry {
    fn is_playlist(&self) -> bool {
        self.uri.starts_with("spotify:playlist:")
    }

    fn title(&self) -> &str {
        self.name.as_deref().unwrap_or("untitled")
    }

    //local files can't be copied
    fn copyable_items(&self) -> Vec<&Item> {
        self.items
            .iter()
            .filter(|i| uri_kind(&i.uri) != "local")
            .collect()
    }
}

#[derive(Default, Serialize, Deserialize)]
struct Progress {
    ///Source playlist uri to its target
    placed: HashMap<String, Placed>,
    sidebar_done: bool,
    library_saved: usize,
    liked_saved: usize,
    pins_done: bool,
}

#[derive(Serialize, Deserialize, Clone, Default)]
struct Placed {
    uri: String,
    is_copy: bool,
    copied_items: usize,
    #[serde(default)]
    cover_done: bool,
}

struct ProgressFile {
    path: PathBuf,
    state: Progress,
}

impl ProgressFile {
    fn path_for(backup: &Path) -> PathBuf {
        backup.with_extension("progress.json")
    }

    fn load(backup: &Path) -> Result<Self> {
        let path = Self::path_for(backup);
        let state = match std::fs::read(&path) {
            Ok(bytes) => serde_json::from_slice(&bytes)
                .with_context(|| format!("reading {}", path.display()))?,
            Err(_) => Progress::default(),
        };
        Ok(Self { path, state })
    }

    fn save(&self) -> Result<()> {
        std::fs::write(&self.path, serde_json::to_vec_pretty(&self.state)?)
            .with_context(|| format!("writing {}", self.path.display()))
    }
}

//captures auth headers from inside the page
const RECORDER: &str = r#"(() => {
  const seen = (window.__spotifyMove = { headers: null, at: 0 });
  const note = (url, headers) => {
    if (!url.startsWith('https://api-partner.spotify.com/pathfinder/')) return;
    const h = {};
    new Headers(headers).forEach((v, k) => (h[k] = v));
    if (h.authorization && h['client-token']) Object.assign(seen, { headers: h, at: Date.now() });
  };
  const fetch = window.fetch;
  window.fetch = function (input, init) {
    try {
      const url = typeof input === 'string' ? input : input.href ?? input.url;
      note(url, init?.headers ?? (input instanceof Request ? input.headers : undefined));
    } catch {}
    return fetch.apply(this, arguments);
  };
  const { open, setRequestHeader, send } = XMLHttpRequest.prototype;
  XMLHttpRequest.prototype.open = function (method, url) {
    this.__sm = { url: String(url), headers: {} };
    return open.apply(this, arguments);
  };
  XMLHttpRequest.prototype.setRequestHeader = function (k, v) {
    if (this.__sm) this.__sm.headers[k] = v;
    return setRequestHeader.apply(this, arguments);
  };
  XMLHttpRequest.prototype.send = function () {
    try { if (this.__sm) note(this.__sm.url, this.__sm.headers); } catch {}
    return send.apply(this, arguments);
  };
})();"#;

#[derive(Default, Clone, Deserialize)]
struct Captured {
    headers: Option<Value>,
    ///Page clock in ms since the epoch
    at: u64,
}

impl Captured {
    //None if no response
    async fn read(page: &Page) -> Option<Captured> {
        let eval = page.evaluate("JSON.stringify(window.__spotifyMove ?? {})");
        let json: String = tokio::time::timeout(Duration::from_secs(5), eval)
            .await
            .ok()?
            .ok()?
            .into_value()
            .ok()?;
        serde_json::from_str(&json).ok()
    }

    fn age(&self) -> Duration {
        Duration::from_millis(now_ms().saturating_sub(self.at))
    }
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

struct Operation {
    mutation: bool,
    hash: String,
}

#[derive(Clone, Copy)]
enum Body<'a> {
    None,
    Json(&'a Value),
    Jpeg(&'a [u8]),
}

//logged-in web player
struct Session {
    browser: Browser,
    page: Mutex<Page>,
    auth: Mutex<Captured>,
    http: reqwest::Client,
    operations: HashMap<String, Operation>,
    user: String,
    read_only: bool,
}

impl Session {
    async fn open(profile: &Path, chrome: Option<&Path>, read_only: bool) -> Result<Self> {
        std::fs::create_dir_all(profile)?;
        let mut config = BrowserConfig::builder()
            .with_head()
            .viewport(None)
            .user_data_dir(profile);
        if let Some(chrome) = chrome {
            config = config.chrome_executable(chrome);
        }
        let (browser, mut handler) = Browser::launch(config.build().map_err(|e| anyhow!(e))?)
            .await
            .context("starting Chrome (use --chrome to point at it)")?;
        //drain errors from unparseable CDP messages
        tokio::spawn(async move { while handler.next().await.is_some() {} });

        let page = open_tab(&browser).await?;
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(60))
            .build()?;
        let operations = load_operations(&http).await?;
        let mut session = Self {
            browser,
            page: Mutex::new(page),
            auth: Mutex::default(),
            http,
            operations,
            user: String::new(),
            read_only,
        };
        println!("Waiting for Spotify in the Chrome window. Log in there if it asks.");
        session.user = session.wait_for_login().await?;
        println!("Logged in as {}", session.user);
        Ok(session)
    }

    fn page(&self) -> Page {
        self.page.lock().unwrap().clone()
    }

    async fn replace_tab(&self) -> Result<Page> {
        let old = self.page();
        let page = open_tab(&self.browser).await?;
        *self.page.lock().unwrap() = page.clone();
        let _ = old.close().await;
        Ok(page)
    }

    async fn wait_for_login(&self) -> Result<String> {
        let started = Instant::now();
        let mut answered = started;
        let mut reloaded = started;
        loop {
            sleep(Duration::from_secs(2)).await;
            if started.elapsed() > LOGIN_TIMEOUT {
                bail!(
                    "nobody logged in within {} minutes",
                    LOGIN_TIMEOUT.as_secs() / 60
                );
            }
            let page = self.page();
            let Some(captured) = Captured::read(&page).await else {
                //chrome sometimes leaves a new tab paused
                if answered.elapsed() > PAGE_STALL {
                    self.replace_tab().await?;
                    answered = Instant::now();
                }
                continue;
            };
            answered = Instant::now();
            if let Some(user) = self.whoami(&captured).await {
                *self.auth.lock().unwrap() = captured;
                return Ok(user);
            }
            let on_player = page
                .url()
                .await
                .ok()
                .flatten()
                .is_some_and(|u| u.starts_with(WEB_PLAYER));
            if on_player && reloaded.elapsed() > Duration::from_secs(30) {
                let _ = page.reload().await;
                reloaded = Instant::now();
            }
        }
    }

    //one attempt to avoid reloads
    async fn whoami(&self, captured: &Captured) -> Option<String> {
        let headers = auth_headers(captured.headers.as_ref()?)?;
        let op = self.operations.get("profileAttributes")?;
        let body = json!({
            "variables": {},
            "operationName": "profileAttributes",
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": op.hash } },
        });
        let mut req = self.http.post(PATHFINDER).json(&body);
        for (k, v) in headers {
            req = req.header(k, v);
        }
        let res: Value = req.send().await.ok()?.json().await.ok()?;
        str_at(&res, "/data/me/profile/username")
    }

    //close cleanly so chrome saves the cookies
    async fn close(mut self) {
        let _ = self.browser.close().await;
        let _ = self.browser.wait().await;
    }

    async fn refresh(&self) -> Result<()> {
        let before = self.auth.lock().unwrap().at;
        for fresh_tab in [false, true] {
            let page = if fresh_tab {
                self.replace_tab().await?
            } else {
                let page = self.page();
                let _ = page.reload().await;
                page
            };
            for _ in 0..30 {
                sleep(Duration::from_secs(1)).await;
                if let Some(c) = Captured::read(&page).await
                    && c.at > before
                    && c.headers.is_some()
                {
                    *self.auth.lock().unwrap() = c;
                    return Ok(());
                }
            }
        }
        bail!("the web player stopped responding, run the same command again to resume")
    }

    ///`Ok(None)` on 404
    async fn send(&self, method: Method, url: &str, body: Body<'_>) -> Result<Option<Value>> {
        if self.read_only && method != Method::GET && url != PATHFINDER {
            bail!("refusing {method} {url}: this session is read-only");
        }
        for attempt in 1..=8u64 {
            let auth = self.auth.lock().unwrap().clone();
            if auth.age() > TOKEN_MAX_AGE {
                self.refresh().await?;
                continue;
            }
            let headers = auth
                .headers
                .as_ref()
                .and_then(auth_headers)
                .ok_or_else(|| anyhow!("lost the web player's login headers"))?;
            let mut req = self
                .http
                .request(method.clone(), url)
                .header("accept", "application/json");
            for (k, v) in &headers {
                req = req.header(k, v);
            }
            req = match body {
                Body::None => req,
                Body::Json(v) => req.json(v),
                Body::Jpeg(bytes) => req
                    .header("content-type", "image/jpeg")
                    .body(bytes.to_vec()),
            };
            let res = match req.send().await {
                Ok(res) => res,
                Err(e) => {
                    eprintln!("  {e}; retrying");
                    sleep(Duration::from_secs(2 * attempt)).await;
                    continue;
                }
            };
            match res.status() {
                StatusCode::NOT_FOUND => return Ok(None),
                StatusCode::UNAUTHORIZED => self.refresh().await?,
                StatusCode::TOO_MANY_REQUESTS => {
                    let wait = res
                        .headers()
                        .get("retry-after")
                        .and_then(|v| v.to_str().ok()?.parse().ok())
                        .unwrap_or(5);
                    eprintln!("  rate limited, waiting {wait}s");
                    sleep(Duration::from_secs(wait) + Duration::from_millis(500)).await;
                }
                s if s.is_server_error() => sleep(Duration::from_secs(2 * attempt)).await,
                s if s.is_success() => {
                    let text = res.text().await?;
                    return Ok(Some(if text.is_empty() {
                        Value::Null
                    } else {
                        serde_json::from_str(&text)?
                    }));
                }
                s => bail!(
                    "{method} {url}: {s}: {}",
                    res.text().await.unwrap_or_default()
                ),
            }
        }
        bail!("{method} {url}: giving up after repeated failures")
    }

    async fn get(&self, url: &str) -> Result<Option<Value>> {
        self.send(Method::GET, url, Body::None).await
    }

    async fn post(&self, url: &str, body: Value) -> Result<Value> {
        self.send(Method::POST, url, Body::Json(&body))
            .await?
            .ok_or_else(|| anyhow!("POST {url}: not found"))
    }

    async fn query(&self, name: &str, variables: Value) -> Result<Value> {
        let op = self
            .operations
            .get(name)
            .ok_or_else(|| anyhow!("the web player has no `{name}` operation anymore"))?;
        if self.read_only && op.mutation {
            bail!("refusing {name}: this session is read-only");
        }
        let body = json!({
            "variables": variables,
            "operationName": name,
            "extensions": { "persistedQuery": { "version": 1, "sha256Hash": op.hash } },
        });
        let mut res = self.post(PATHFINDER, body).await?;
        if res["errors"].as_array().is_some_and(|e| !e.is_empty()) {
            bail!("{name}: {}", res["errors"]);
        }
        Ok(res["data"].take())
    }

    async fn playlist(&self, uri: &str, length: usize) -> Result<Option<Value>> {
        self.get(&format!(
            "{PLAYLIST_API}/playlist/{}?{DECORATE}&from=0&length={length}",
            uri_id(uri)
        ))
        .await
    }

    async fn sidebar(&self) -> Result<Vec<Value>> {
        let url = format!(
            "{PLAYLIST_API}/user/{}/rootlist?{DECORATE}&from=0&length=100000",
            percent_encode(&self.user)
        );
        let mut rootlist = self.get(&url).await?.unwrap_or(Value::Null);
        Ok(rootlist["contents"]["items"]
            .as_array_mut()
            .map(std::mem::take)
            .unwrap_or_default())
    }

    async fn upload_cover(&self, playlist: &str, jpeg: &[u8]) -> Result<()> {
        let upload = self
            .send(Method::POST, IMAGE_UPLOAD, Body::Jpeg(jpeg))
            .await?
            .unwrap_or_default();
        let token = str_at(&upload, "/uploadToken").ok_or_else(|| anyhow!("no upload token"))?;
        let id = uri_id(playlist);
        let registered = self
            .post(
                &format!("{PLAYLIST_API}/playlist/{id}/register-image"),
                json!({ "uploadToken": token }),
            )
            .await?;
        let picture = str_at(&registered, "/picture").ok_or_else(|| anyhow!("no image id"))?;
        let op = json!({ "kind": "UPDATE_LIST_ATTRIBUTES",
            "updateListAttributes": { "newAttributes": { "values": { "picture": picture } } } });
        self.post(
            &format!("{PLAYLIST_API}/playlist/{id}/changes"),
            change(json!([op])),
        )
        .await?;
        Ok(())
    }

    async fn save(&self, uris: &[String]) -> Result<()> {
        self.query(
            "addToLibrary",
            json!({ "libraryItemUris": uris, "interactionId": null }),
        )
        .await?;
        Ok(())
    }

    async fn unsave(&self, uris: &[String]) -> Result<()> {
        self.query("removeFromLibrary", json!({ "libraryItemUris": uris }))
            .await?;
        Ok(())
    }
}

async fn open_tab(browser: &Browser) -> Result<Page> {
    let page = browser.new_page("about:blank").await?;
    page.evaluate_on_new_document(RECORDER).await?;
    let url = format!("{WEB_PLAYER}/collection/tracks");
    let _ = tokio::time::timeout(Duration::from_secs(30), page.goto(url)).await;
    Ok(page)
}

fn auth_headers(raw: &Value) -> Option<Vec<(String, String)>> {
    const KEEP: &[&str] = &[
        "authorization",
        "client-token",
        "spotify-app-version",
        "app-platform",
        "accept-language",
    ];
    let mut out: Vec<(String, String)> = raw
        .as_object()?
        .iter()
        .filter_map(|(k, v)| {
            let k = k.to_ascii_lowercase();
            KEEP.contains(&k.as_str())
                .then(|| (k, v.as_str().unwrap_or_default().to_owned()))
        })
        .collect();
    let has = |name: &str| out.iter().any(|(k, _)| k == name);
    if !has("authorization") || !has("client-token") {
        return None;
    }
    out.push(("origin".into(), WEB_PLAYER.into()));
    out.push(("referer".into(), format!("{WEB_PLAYER}/")));
    Some(out)
}

///Reads persisted query hashes from the web player's scripts
async fn load_operations(http: &reqwest::Client) -> Result<HashMap<String, Operation>> {
    let ua = "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/140.0 Safari/537.36";
    let html = http
        .get(WEB_PLAYER)
        .header("user-agent", ua)
        .send()
        .await?
        .text()
        .await?;
    let script = Regex::new(r#"src="(https://open\.spotifycdn\.com/[^"]+\.js)""#)?;
    let definition = Regex::new(r#""(\w+)","(query|mutation)","([0-9a-f]{64})""#)?;
    let mut ops = HashMap::new();
    for src in script.captures_iter(&html) {
        let js = http.get(&src[1]).send().await?.text().await?;
        for m in definition.captures_iter(&js) {
            ops.insert(
                m[1].to_owned(),
                Operation {
                    mutation: &m[2] == "mutation",
                    hash: m[3].to_owned(),
                },
            );
        }
    }
    if ops.is_empty() {
        bail!(
            "no GraphQL operations found in the web player's scripts"
        );
    }
    Ok(ops)
}

fn percent_encode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

//`None` means the automatic mosaic
fn cover_url(playlist: &Value) -> Option<String> {
    let attrs = &playlist["attributes"];
    if let Some(id) = attrs["picture"].as_str() {
        let mut hex: String = BASE64
            .decode(id)
            .ok()?
            .iter()
            .map(|b| format!("{b:02x}"))
            .collect();
        //bebb is the 640px rendition
        if hex.len() == 40 && hex.starts_with("ab67") {
            hex.replace_range(12..16, "bebb");
        }
        return Some(format!("https://i.scdn.co/image/{hex}"));
    }
    attrs["pictureSize"]
        .as_array()?
        .iter()
        .find(|p| p["targetName"] == "default")
        .and_then(|p| str_at(p, "/url"))
}

fn str_at(v: &Value, pointer: &str) -> Option<String> {
    v.pointer(pointer)
        .and_then(Value::as_str)
        .map(str::to_owned)
}

fn uri_kind(uri: &str) -> &str {
    uri.split(':').nth(1).unwrap_or_default()
}

fn uri_id(uri: &str) -> &str {
    uri.rsplit(':').next().unwrap_or_default()
}

async fn read_liked(s: &Session) -> Result<Vec<Saved>> {
    let collection = format!("spotify:user:{}:collection", s.user);
    let mut liked = Vec::new();
    loop {
        let data = s
            .query(
                "fetchLibraryTracks",
                json!({ "uri": collection, "offset": liked.len(), "limit": 500 }),
            )
            .await?;
        let page = &data["me"]["library"]["tracks"];
        let items = page["items"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        liked.extend(items.iter().map(|it| Saved {
            uri: str_at(it, "/track/_uri").unwrap_or_default(),
            name: str_at(it, "/track/data/name"),
            added_at: str_at(it, "/addedAt/isoString"),
        }));
        let total = page["totalCount"].as_u64().unwrap_or(0) as usize;
        if items.is_empty() || liked.len() >= total {
            return Ok(liked);
        }
    }
}

///Saved albums, artists and shows, and pins
async fn read_library(s: &Session) -> Result<(Vec<Saved>, Vec<String>)> {
    let mut saved = Vec::new();
    let mut pinned = Vec::new();
    let mut offset = 0;
    loop {
        let vars = json!({
            "order": null, "textFilter": "", "features": ["LIKED_SONGS", "YOUR_EPISODES_V2"],
            "limit": 50, "offset": offset, "flatten": true, "expandedFolders": [],
            "folderUri": null, "includeFoldersWhenFlattening": true,
        });
        let data = s.query("libraryV3", vars).await?;
        let lib = &data["me"]["libraryV3"];
        let items = lib["items"]
            .as_array()
            .map(Vec::as_slice)
            .unwrap_or_default();
        offset += items.len();
        for it in items {
            let uri = str_at(it, "/item/_uri").unwrap_or_default();
            if it["pinned"].as_bool() == Some(true) {
                pinned.push(uri.clone());
            }
            if LIBRARY_KINDS.contains(&uri_kind(&uri)) {
                let name =
                    str_at(it, "/item/data/name").or_else(|| str_at(it, "/item/data/profile/name"));
                saved.push(Saved {
                    uri,
                    name,
                    added_at: str_at(it, "/addedAt/isoString"),
                });
            }
        }
        if items.is_empty() || offset as u64 >= lib["totalCount"].as_u64().unwrap_or(0) {
            return Ok((saved, pinned));
        }
    }
}

fn parse_items(playlist: &Value) -> Result<Vec<Item>> {
    if playlist["contents"]["truncated"].as_bool() == Some(true) {
        bail!("Spotify returned only part of the playlist");
    }
    let items = playlist["contents"]["items"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default();
    Ok(items
        .iter()
        .map(|i| Item {
            uri: str_at(i, "/uri").unwrap_or_default(),
            added_at: str_at(i, "/attributes/timestamp"),
        })
        .collect())
}

async fn read_playlists(s: &Session) -> Result<Vec<Entry>> {
    let mut entries = Vec::new();
    for row in s.sidebar().await? {
        let mut entry = Entry {
            uri: str_at(&row, "/uri").unwrap_or_default(),
            name: None,
            description: None,
            owner: None,
            public: row["attributes"]["public"].as_bool(),
            cover: None,
            items: Vec::new(),
        };
        if entry.is_playlist() {
            let Some(pl) = s.playlist(&entry.uri, 100_000).await? else {
                eprintln!("  skipping {}: Spotify says it no longer exists", entry.uri);
                continue;
            };
            entry.name = str_at(&pl, "/attributes/name");
            entry.description = str_at(&pl, "/attributes/description").filter(|d| !d.is_empty());
            entry.owner = str_at(&pl, "/ownerUsername");
            entry.cover = cover_url(&pl);
            entry.items = parse_items(&pl).with_context(|| format!("reading {}", entry.uri))?;
        }
        entries.push(entry);
    }
    Ok(entries)
}

async fn export(s: &Session) -> Result<Backup> {
    let liked = read_liked(s).await?;
    println!("liked songs: {}", liked.len());
    let (library, pinned) = read_library(s).await?;
    println!("albums, artists and shows: {}", library.len());
    let playlists = read_playlists(s).await?;
    let total = playlists.iter().filter(|p| p.is_playlist()).count();
    let own = playlists
        .iter()
        .filter(|p| p.owner.as_deref() == Some(s.user.as_str()))
        .count();
    println!("playlists: {total}, {own} of them yours");
    Ok(Backup {
        user: s.user.clone(),
        liked,
        library,
        playlists,
        pinned,
    })
}

fn change(ops: Value) -> Value {
    json!({ "deltas": [{ "ops": ops, "info": { "source": { "client": "WEBPLAYER" } } }] })
}

///Copies own and unreachable playlists, follows the rest
async fn place_playlists(s: &Session, backup: &Backup, progress: &mut ProgressFile) -> Result<()> {
    for entry in backup.playlists.iter().filter(|e| e.is_playlist()) {
        let items = entry.copyable_items();
        let mut placed = match progress.state.placed.get(&entry.uri) {
            Some(p) if !p.is_copy || p.copied_items >= items.len() => continue,
            Some(p) => p.clone(),
            None => {
                let own = entry.owner.as_deref() == Some(backup.user.as_str());
                let placed = if !own && s.playlist(&entry.uri, 0).await?.is_some() {
                    Placed {
                        uri: entry.uri.clone(),
                        ..Placed::default()
                    }
                } else {
                    Placed {
                        uri: create_playlist(s, entry).await?,
                        is_copy: true,
                        ..Placed::default()
                    }
                };
                progress
                    .state
                    .placed
                    .insert(entry.uri.clone(), placed.clone());
                progress.save()?;
                if !placed.is_copy {
                    continue;
                }
                placed
            }
        };
        while placed.copied_items < items.len() {
            let batch = &items[placed.copied_items..items.len().min(placed.copied_items + 100)];
            let adds: Vec<Value> = batch
                .iter()
                .map(|i| match &i.added_at {
                    Some(t) => json!({ "uri": i.uri, "attributes": { "timestamp": t } }),
                    None => json!({ "uri": i.uri }),
                })
                .collect();
            let body =
                change(json!([{ "kind": "ADD", "add": { "addLast": true, "items": adds } }]));
            s.post(
                &format!("{PLAYLIST_API}/playlist/{}/changes", uri_id(&placed.uri)),
                body,
            )
            .await?;
            placed.copied_items += batch.len();
            progress
                .state
                .placed
                .insert(entry.uri.clone(), placed.clone());
            progress.save()?;
        }
        println!("  copied {} ({} tracks)", entry.title(), items.len());
    }
    Ok(())
}

async fn copy_covers(s: &Session, backup: &Backup, progress: &mut ProgressFile) -> Result<()> {
    for entry in &backup.playlists {
        let Some(url) = &entry.cover else { continue };
        let Some(mut placed) = progress.state.placed.get(&entry.uri).cloned() else {
            continue;
        };
        if !placed.is_copy || placed.cover_done {
            continue;
        }
        let jpeg = s
            .http
            .get(url)
            .send()
            .await?
            .error_for_status()?
            .bytes()
            .await?;
        s.upload_cover(&placed.uri, &jpeg)
            .await
            .with_context(|| format!("cover for {}", entry.title()))?;
        placed.cover_done = true;
        progress.state.placed.insert(entry.uri.clone(), placed);
        progress.save()?;
        println!("  cover for {}", entry.title());
    }
    Ok(())
}

async fn create_playlist(s: &Session, entry: &Entry) -> Result<String> {
    let mut values = json!({ "name": entry.name.as_deref().unwrap_or("Untitled") });
    if let Some(d) = &entry.description {
        values["description"] = json!(d);
    }
    let op = json!({ "kind": "UPDATE_LIST_ATTRIBUTES", "updateListAttributes": { "newAttributes": { "values": values } } });
    let res = s
        .post(&format!("{PLAYLIST_API}/playlist"), json!({ "ops": [op] }))
        .await?;
    str_at(&res, "/uri").ok_or_else(|| anyhow!("creating {:?} returned no uri", entry.name))
}

fn target_uri<'a>(progress: &'a Progress, source: &'a str) -> &'a str {
    progress
        .placed
        .get(source)
        .map_or(source, |p| p.uri.as_str())
}

async fn arrange_sidebar(s: &Session, backup: &Backup, progress: &mut ProgressFile) -> Result<()> {
    if progress.state.sidebar_done {
        return Ok(());
    }
    let present: HashSet<String> = s
        .sidebar()
        .await?
        .iter()
        .filter_map(|r| str_at(r, "/uri"))
        .collect();
    let now = now_ms().to_string();
    let missing: Vec<Value> = backup
        .playlists
        .iter()
        .filter(|e| !present.contains(target_uri(&progress.state, &e.uri)))
        .map(|e| {
            let mut attrs = json!({ "timestamp": now });
            if e.public == Some(false) {
                attrs["public"] = json!(false);
            }
            json!({ "uri": target_uri(&progress.state, &e.uri), "attributes": attrs })
        })
        .collect();
    if !missing.is_empty() {
        let body =
            change(json!([{ "kind": "ADD", "add": { "addFirst": true, "items": missing } }]));
        s.post(
            &format!(
                "{PLAYLIST_API}/user/{}/rootlist/changes",
                percent_encode(&s.user)
            ),
            body,
        )
        .await?;
    }
    println!("sidebar: added {} entries", missing.len());
    progress.state.sidebar_done = true;
    progress.save()
}

#[derive(Clone, Copy)]
enum Shelf {
    Library,
    Liked,
}

impl Shelf {
    fn label(self) -> &'static str {
        match self {
            Shelf::Library => "albums, artists and shows",
            Shelf::Liked => "liked songs",
        }
    }

    fn saved(self, p: &mut Progress) -> &mut usize {
        match self {
            Shelf::Library => &mut p.library_saved,
            Shelf::Liked => &mut p.liked_saved,
        }
    }

    async fn contents(self, s: &Session) -> Result<HashSet<String>> {
        let items = match self {
            Shelf::Library => read_library(s).await?.0,
            Shelf::Liked => read_liked(s).await?,
        };
        Ok(items.into_iter().map(|x| x.uri).collect())
    }
}

///Saves oldest first, one per second unless `fast`
async fn save_in_order(
    s: &Session,
    shelf: Shelf,
    uris: &[String],
    fast: bool,
    progress: &mut ProgressFile,
) -> Result<()> {
    let start = *shelf.saved(&mut progress.state);
    if start >= uris.len() {
        return Ok(());
    }
    let present = shelf.contents(s).await?;
    let label = shelf.label();
    if fast {
        let fresh: Vec<String> = uris[start..]
            .iter()
            .filter(|u| !present.contains(*u))
            .cloned()
            .collect();
        for batch in fresh.chunks(50) {
            s.save(batch).await?;
        }
        *shelf.saved(&mut progress.state) = uris.len();
        progress.save()?;
        println!("{label}: saved {}", fresh.len());
        return Ok(());
    }
    let remaining = uris.len() - start;
    println!(
        "{label}: saving {remaining}, about {} minutes",
        remaining * 11 / 600 + 1
    );
    let mut last: Option<Instant> = None;
    for (i, uri) in uris.iter().enumerate().skip(start) {
        if let Some(wait) = last.and_then(|t| SAVE_SPACING.checked_sub(t.elapsed())) {
            sleep(wait).await;
        }
        let one = std::slice::from_ref(uri);
        //re-saving alone keeps the old date
        if present.contains(uri) {
            s.unsave(one).await?;
        }
        s.save(one).await?;
        last = Some(Instant::now());
        *shelf.saved(&mut progress.state) = i + 1;
        if (i + 1) % 25 == 0 || i + 1 == uris.len() {
            progress.save()?;
        }
        if (i + 1) % 500 == 0 {
            println!("  {}/{}", i + 1, uris.len());
        }
    }
    Ok(())
}

///Orders by date saved, oldest first
fn oldest_first(items: &[Saved]) -> Vec<String> {
    let mut sorted: Vec<&Saved> = items.iter().collect();
    sorted.sort_by(|a, b| a.added_at.cmp(&b.added_at));
    sorted.into_iter().map(|s| s.uri.clone()).collect()
}

async fn restore_pins(s: &Session, backup: &Backup, progress: &mut ProgressFile) -> Result<()> {
    if progress.state.pins_done {
        return Ok(());
    }
    let (_, already) = read_library(s).await?;
    for uri in &backup.pinned {
        let target = target_uri(&progress.state, uri);
        if already.iter().any(|a| a == target) {
            continue;
        }
        if let Err(e) = s
            .query("pinLibraryItem", json!({ "pinnableItemUri": target }))
            .await
        {
            eprintln!("  could not pin {target}: {e}");
        }
    }
    progress.state.pins_done = true;
    progress.save()
}

async fn import(
    s: &Session,
    backup: &Backup,
    progress: &mut ProgressFile,
    fast: bool,
) -> Result<()> {
    if s.user == backup.user {
        bail!("this is the account the backup came from; log in to the other one");
    }
    place_playlists(s, backup, progress).await?;
    copy_covers(s, backup, progress).await?;
    arrange_sidebar(s, backup, progress).await?;
    let library = oldest_first(&backup.library);
    save_in_order(s, Shelf::Library, &library, fast, progress).await?;
    let liked: Vec<String> = backup.liked.iter().rev().map(|x| x.uri.clone()).collect();
    save_in_order(s, Shelf::Liked, &liked, fast, progress).await?;
    restore_pins(s, backup, progress).await
}

fn report(ok: bool, what: &str, detail: String) -> bool {
    println!(
        "{} {what}{}",
        if ok { "ok      " } else { "MISMATCH" },
        if detail.is_empty() {
            detail
        } else {
            format!(": {detail}")
        }
    );
    ok
}

///Describes anything missing or out of order ignoring extras
fn order_problem(want: &[String], got: &[String]) -> String {
    let have: HashSet<&String> = got.iter().collect();
    let missing = want.iter().filter(|u| !have.contains(u)).count();
    let wanted: HashSet<&String> = want.iter().collect();
    if missing > 0 {
        format!("{missing} missing")
    } else if !got.iter().filter(|u| wanted.contains(u)).eq(want.iter()) {
        "order differs".into()
    } else {
        String::new()
    }
}

fn same_items(got: &[Item], want: &[&Item]) -> bool {
    got.len() == want.len()
        && got
            .iter()
            .zip(want)
            .all(|(g, w)| g.uri == w.uri && (w.added_at.is_none() || g.added_at == w.added_at))
}

async fn verify(s: &Session, backup: &Backup, progress: &Progress) -> Result<bool> {
    let mut all_ok = true;

    let want: Vec<String> = backup.liked.iter().map(|x| x.uri.clone()).collect();
    let got: Vec<String> = read_liked(s).await?.into_iter().map(|x| x.uri).collect();
    let detail = order_problem(&want, &got);
    all_ok &= report(
        detail.is_empty(),
        &format!("{} liked songs", want.len()),
        detail,
    );

    let (library, pinned) = read_library(s).await?;
    let have: HashSet<&String> = library.iter().map(|x| &x.uri).collect();
    let missing: Vec<&str> = backup
        .library
        .iter()
        .filter(|x| !have.contains(&x.uri))
        .map(|x| x.name.as_deref().unwrap_or(&x.uri))
        .collect();
    all_ok &= report(
        missing.is_empty(),
        &format!("{} albums, artists and shows", backup.library.len()),
        missing.join(", "),
    );

    let want: Vec<String> = backup
        .playlists
        .iter()
        .map(|e| target_uri(progress, &e.uri).to_owned())
        .collect();
    let got: Vec<String> = s
        .sidebar()
        .await?
        .iter()
        .filter_map(|r| str_at(r, "/uri"))
        .collect();
    let detail = order_problem(&want, &got);
    all_ok &= report(
        detail.is_empty(),
        &format!("{} sidebar entries", want.len()),
        detail,
    );

    let copies: Vec<&Entry> = backup
        .playlists
        .iter()
        .filter(|e| progress.placed.get(&e.uri).is_some_and(|p| p.is_copy))
        .collect();
    let mut differing = Vec::new();
    let mut coverless = Vec::new();
    for entry in &copies {
        let target = target_uri(progress, &entry.uri);
        let pl = s.playlist(target, 100_000).await?.unwrap_or(Value::Null);
        let same = str_at(&pl, "/attributes/name") == entry.name
            && str_at(&pl, "/attributes/description").filter(|d| !d.is_empty())
                == entry.description
            && same_items(&parse_items(&pl)?, &entry.copyable_items());
        if !same {
            differing.push(entry.title());
        }
        if entry.cover.is_some() && cover_url(&pl).is_none() {
            coverless.push(entry.title());
        }
    }
    all_ok &= report(
        differing.is_empty(),
        &format!("{} copied playlists", copies.len()),
        differing.join(", "),
    );
    let covers = copies.iter().filter(|e| e.cover.is_some()).count();
    all_ok &= report(
        coverless.is_empty(),
        &format!("{covers} covers"),
        coverless.join(", "),
    );

    let missing = backup
        .pinned
        .iter()
        .filter(|u| !pinned.iter().any(|p| p == target_uri(progress, u)))
        .count();
    let detail = if missing > 0 {
        format!("{missing} not pinned")
    } else {
        String::new()
    };
    all_ok &= report(
        detail.is_empty(),
        &format!("{} pins", backup.pinned.len()),
        detail,
    );

    Ok(all_ok)
}

fn read_backup(path: &Path) -> Result<Backup> {
    let bytes = std::fs::read(path).with_context(|| format!("reading {}", path.display()))?;
    serde_json::from_slice(&bytes).with_context(|| format!("parsing {}", path.display()))
}

async fn run(cli: Cli) -> Result<bool> {
    let chrome = cli.chrome.as_deref();
    match cli.cmd {
        Cmd::Export { profile, out } => {
            let s = Session::open(&cli.profiles.join(&profile), chrome, true).await?;
            let result = export(&s).await;
            s.close().await;
            let backup = result?;
            let out = out.unwrap_or_else(|| PathBuf::from(format!("library-{}.json", backup.user)));
            std::fs::write(&out, serde_json::to_vec_pretty(&backup)?)?;
            println!("wrote {}", out.display());
            Ok(true)
        }
        Cmd::Import {
            profile,
            backup: path,
            fast,
        } => {
            let backup = read_backup(&path)?;
            let mut progress = ProgressFile::load(&path)?;
            let s = Session::open(&cli.profiles.join(&profile), chrome, false).await?;
            let result = async {
                import(&s, &backup, &mut progress, fast).await?;
                println!("checking the result:");
                verify(&s, &backup, &progress.state).await
            }
            .await;
            s.close().await;
            result
        }
        Cmd::Verify {
            profile,
            backup: path,
        } => {
            let backup = read_backup(&path)?;
            let progress = ProgressFile::load(&path)?;
            if progress.state.placed.is_empty() {
                bail!(
                    "no {} found; verify needs the progress file import wrote",
                    progress.path.display()
                );
            }
            let s = Session::open(&cli.profiles.join(&profile), chrome, true).await?;
            let result = verify(&s, &backup, &progress.state).await;
            s.close().await;
            result
        }
    }
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    match run(Cli::parse()).await {
        Ok(true) => std::process::ExitCode::SUCCESS,
        Ok(false) => std::process::ExitCode::from(2),
        Err(e) => {
            eprintln!("error: {e:#}");
            std::process::ExitCode::FAILURE
        }
    }
}
