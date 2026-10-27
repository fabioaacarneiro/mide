//! Players de mídia pelo MPRIS (Spotify, navegadores, mpv, VLC, Rhythmbox...): o
//! "Tocando Agora" da barra (B29).
//!
//! Cada player tem um nome `org.mpris.MediaPlayer2.*` no barramento da sessão e o
//! objeto `/org/mpris/MediaPlayer2` com duas interfaces: `org.mpris.MediaPlayer2`
//! (nome do app, `.desktop`, `Raise`) e `org.mpris.MediaPlayer2.Player` (estado,
//! música, comandos). O módulo acompanha os players que aparecem e somem
//! (`NameOwnerChanged`), relê as propriedades a cada `PropertiesChanged` e guarda a
//! posição do `Seeked`. A posição (`Position`) não é avisada pelos players: ela é lida
//! quando pedida (`refresh_position`) e estimada pelo tempo passado enquanto toca.
//!
//! Os comandos (tocar/pausar, próxima, anterior, posição, volume) vão numa thread, para
//! a barra nunca esperar o player. O player "principal" é o último que começou a tocar
//! (ou o que o usuário escolheu depois disso), como no macOS.

use std::{
    collections::HashMap,
    path::PathBuf,
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc, Mutex,
    },
    time::Instant,
};

use zbus::zvariant::{OwnedValue, Value};

const PREFIX: &str = "org.mpris.MediaPlayer2.";
const PATH: &str = "/org/mpris/MediaPlayer2";
const PLAYER: &str = "org.mpris.MediaPlayer2.Player";
const ROOT: &str = "org.mpris.MediaPlayer2";

/// Estado de reprodução (`PlaybackStatus`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Status {
    Playing,
    Paused,
    #[default]
    Stopped,
}

impl Status {
    pub fn parse(text: &str) -> Self {
        match text {
            "Playing" => Status::Playing,
            "Paused" => Status::Paused,
            _ => Status::Stopped,
        }
    }
}

/// A música (ou vídeo) do player (`Metadata`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Track {
    /// `mpris:trackid` (objeto), usado no `SetPosition`.
    pub id: String,
    pub title: String,
    /// Artistas juntos por vírgula (`xesam:artist` é uma lista).
    pub artist: String,
    pub album: String,
    /// `mpris:artUrl`: `file://`, `https://` ou `http://`.
    pub art_url: String,
    /// Duração em microssegundos (`mpris:length`), se o player disser.
    pub length_us: Option<i64>,
    /// `xesam:url` (o arquivo ou endereço tocando): o título de reserva.
    pub url: String,
}

impl Track {
    /// Título para mostrar: o da música ou, sem ele, o nome do arquivo tocando.
    pub fn display_title(&self) -> String {
        if !self.title.trim().is_empty() {
            return self.title.trim().to_string();
        }
        let name = self.url.rsplit('/').next().unwrap_or("");
        percent_decode(name)
    }
}

/// Um player no barramento.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Player {
    /// Nome no barramento (`org.mpris.MediaPlayer2.spotify`) e o dono único dele.
    pub bus: String,
    pub owner: String,
    /// `Identity` (nome do app) e `DesktopEntry` (o `.desktop`, sem a extensão).
    pub identity: String,
    pub desktop_entry: String,
    pub status: Status,
    pub track: Track,
    pub can_next: bool,
    pub can_previous: bool,
    pub can_play: bool,
    pub can_pause: bool,
    pub can_seek: bool,
    pub can_control: bool,
    pub can_raise: bool,
    /// Volume do próprio player (0 a 1), quando ele tem.
    pub volume: Option<f64>,
    /// Velocidade (`Rate`, 1 por padrão).
    pub rate: f64,
    /// Última posição lida (microssegundos) e quando foi lida.
    pub position_us: i64,
    pub position_at: Option<Instant>,
    /// Ordem em que começou a tocar (maior = mais recente; 0 = nunca tocou aqui).
    pub played: u64,
}

impl Player {
    /// Nome para mostrar: o `Identity` ou, sem ele, o fim do nome no barramento.
    pub fn name(&self) -> String {
        if !self.identity.is_empty() {
            return self.identity.clone();
        }
        let short = self.bus.strip_prefix(PREFIX).unwrap_or(&self.bus);
        let short = short.split('.').next().unwrap_or(short);
        let mut chars = short.chars();
        chars.next().map(|c| c.to_uppercase().collect::<String>() + chars.as_str()).unwrap_or_default()
    }

    /// Posição agora (microssegundos): a última lida mais o tempo tocando desde então.
    pub fn position_now(&self) -> i64 {
        let mut pos = self.position_us;
        if let (Status::Playing, Some(at)) = (self.status, self.position_at) {
            pos += (at.elapsed().as_micros() as f64 * self.rate.max(0.0)) as i64;
        }
        match self.track.length_us {
            Some(len) if len > 0 => pos.clamp(0, len),
            _ => pos.max(0),
        }
    }
}

/// Lê o `Metadata` de um player (dicionário `a{sv}`).
pub fn parse_metadata(meta: &HashMap<String, OwnedValue>) -> Track {
    let get = |key: &str| meta.get(key).map(|v| &**v);
    Track {
        id: get("mpris:trackid").and_then(text).unwrap_or_default(),
        title: get("xesam:title").and_then(text).unwrap_or_default(),
        artist: get("xesam:artist").map(texts).unwrap_or_default().join(", "),
        album: get("xesam:album").and_then(text).unwrap_or_default(),
        art_url: get("mpris:artUrl").and_then(text).unwrap_or_default(),
        length_us: get("mpris:length").and_then(number).filter(|l| *l > 0),
        url: get("xesam:url").and_then(text).unwrap_or_default(),
    }
}

/// Tira o envelope de variante (`v`) que os dicionários `a{sv}` deixam.
fn inner<'a>(value: &'a Value<'a>) -> &'a Value<'a> {
    match value {
        Value::Value(boxed) => inner(boxed),
        other => other,
    }
}

fn text(value: &Value) -> Option<String> {
    match inner(value) {
        Value::Str(s) => Some(s.to_string()),
        Value::ObjectPath(p) => Some(p.to_string()),
        // Alguns players mandam o título como lista, como o artista.
        Value::Array(_) => texts(value).into_iter().next(),
        _ => None,
    }
}

fn texts(value: &Value) -> Vec<String> {
    match inner(value) {
        Value::Array(items) => items.iter().filter_map(text).filter(|s| !s.is_empty()).collect(),
        other => text(other).into_iter().filter(|s| !s.is_empty()).collect(),
    }
}

/// Número inteiro de qualquer tipo (os players mandam `mpris:length` como `x`, `t`, `i`
/// ou até `d`).
fn number(value: &Value) -> Option<i64> {
    match inner(value) {
        Value::I64(n) => Some(*n),
        Value::U64(n) => i64::try_from(*n).ok(),
        Value::I32(n) => Some(*n as i64),
        Value::U32(n) => Some(*n as i64),
        Value::I16(n) => Some(*n as i64),
        Value::U16(n) => Some(*n as i64),
        Value::U8(n) => Some(*n as i64),
        Value::F64(n) if n.is_finite() => Some(*n as i64),
        _ => None,
    }
}

fn float(value: &Value) -> Option<f64> {
    match inner(value) {
        Value::F64(n) if n.is_finite() => Some(*n),
        other => number(other).map(|n| n as f64),
    }
}

fn boolean(value: &Value) -> Option<bool> {
    match inner(value) {
        Value::Bool(b) => Some(*b),
        _ => None,
    }
}

/// `%20` e afins num nome de arquivo de `xesam:url`.
fn percent_decode(text: &str) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'%' && i + 2 < bytes.len() && text.is_char_boundary(i + 3) {
            if let Ok(b) = u8::from_str_radix(&text[i + 1..i + 3], 16) {
                out.push(b);
                i += 3;
                continue;
            }
        }
        out.push(bytes[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Qual player é o principal: o escolhido pelo usuário, se foi escolhido depois do
/// último que começou a tocar; senão, o último que começou a tocar. Sem nenhum que
/// tenha tocado, o primeiro tocando, depois o primeiro pausado, depois o primeiro.
/// `chosen` é o nome no barramento e a ordem em que foi escolhido.
pub fn choose(players: &[Player], chosen: Option<&(String, u64)>) -> Option<usize> {
    if players.is_empty() {
        return None;
    }
    let rank = |p: &Player| match p.status {
        Status::Playing => 2,
        Status::Paused => 1,
        Status::Stopped => 0,
    };
    let key = |p: &Player| {
        let picked = chosen.filter(|(bus, _)| *bus == p.bus).map(|(_, at)| *at).unwrap_or(0);
        (p.played.max(picked), rank(p))
    };
    // `max_by_key` devolve o último dos iguais: percorrer ao contrário fica com o
    // primeiro da lista no empate.
    players.iter().enumerate().rev().max_by_key(|(_, p)| key(p)).map(|(i, _)| i)
}

/// Tempo como no Mac: "3:07", "1:02:03".
pub fn format_time(us: i64) -> String {
    let secs = (us.max(0) / 1_000_000) as u64;
    let (h, m, s) = (secs / 3600, secs / 60 % 60, secs % 60);
    if h > 0 {
        format!("{h}:{m:02}:{s:02}")
    } else {
        format!("{m}:{s:02}")
    }
}

/// Os players acompanhados, compartilhados com quem mostra (a barra). `version` muda a
/// cada mudança (quem mostra redesenha quando ela muda).
#[derive(Clone)]
pub struct Mpris {
    players: Arc<Mutex<Vec<Player>>>,
    chosen: Arc<Mutex<Option<(String, u64)>>>,
    order: Arc<AtomicU64>,
    version: Arc<AtomicU64>,
    conn: zbus::blocking::Connection,
}

impl Mpris {
    pub fn version(&self) -> u64 {
        self.version.load(Ordering::Relaxed)
    }

    fn changed(&self) {
        self.version.fetch_add(1, Ordering::Relaxed);
    }

    /// Todos os players, na ordem em que apareceram.
    pub fn players(&self) -> Vec<Player> {
        self.players.lock().map(|p| p.clone()).unwrap_or_default()
    }

    /// O player principal (ver `choose`).
    pub fn main(&self) -> Option<Player> {
        let players = self.players();
        let chosen = self.chosen.lock().ok().and_then(|c| c.clone());
        choose(&players, chosen.as_ref()).map(|i| players[i].clone())
    }

    /// O usuário escolheu este player no painel: vira o principal até outro começar a
    /// tocar.
    pub fn select(&self, bus: &str) {
        let at = self.order.fetch_add(1, Ordering::Relaxed) + 1;
        if let Ok(mut chosen) = self.chosen.lock() {
            *chosen = Some((bus.to_string(), at));
        }
        self.changed();
    }

    /// Chama um método do player numa thread (`PlayPause`, `Next`, `Previous`...).
    pub fn command(&self, bus: &str, method: &'static str) {
        self.call(bus, PLAYER, method, ());
    }

    /// Traz a janela do player para a frente (`Raise`).
    pub fn raise(&self, bus: &str) {
        self.call(bus, ROOT, "Raise", ());
    }

    /// Vai para a posição `us` da música (`SetPosition`, que pede o id da música).
    /// Sem id, usa o `Seek` relativo à posição estimada.
    pub fn set_position(&self, player: &Player, us: i64) {
        let us = match player.track.length_us {
            Some(len) => us.clamp(0, len),
            None => us.max(0),
        };
        // A posição nova já vale para o desenho, sem esperar a releitura.
        if let Ok(mut players) = self.players.lock() {
            if let Some(p) = players.iter_mut().find(|p| p.bus == player.bus) {
                p.position_us = us;
                p.position_at = Some(Instant::now());
            }
        }
        self.changed();
        match zbus::zvariant::ObjectPath::try_from(player.track.id.clone()) {
            Ok(id) if !player.track.id.is_empty() => {
                self.call(&player.bus, PLAYER, "SetPosition", (zbus::zvariant::OwnedObjectPath::from(id), us));
            }
            _ => self.call(&player.bus, PLAYER, "Seek", (us - player.position_now(),)),
        }
    }

    /// Volume do player (0 a 1), pela propriedade `Volume`.
    pub fn set_volume(&self, bus: &str, volume: f64) {
        let volume = volume.clamp(0.0, 1.0);
        if let Ok(mut players) = self.players.lock() {
            if let Some(p) = players.iter_mut().find(|p| p.bus == bus) {
                p.volume = Some(volume);
            }
        }
        self.changed();
        let (conn, bus) = (self.conn.clone(), bus.to_string());
        std::thread::spawn(move || {
            let set = (|| -> zbus::Result<()> {
                let props = zbus::blocking::fdo::PropertiesProxy::builder(&conn).destination(bus.clone())?.path(PATH)?.build()?;
                let iface = zbus::names::InterfaceName::try_from(PLAYER)?;
                props.set(iface, "Volume", Value::F64(volume))?;
                Ok(())
            })();
            if let Err(err) = set {
                tracing::debug!("mpris: volume de {bus} falhou: {err}");
            }
        });
    }

    /// Relê a posição (`Position`) do player numa thread.
    pub fn refresh_position(&self, bus: &str) {
        let (me, bus) = (self.clone(), bus.to_string());
        std::thread::spawn(move || {
            let Some(pos) = read_position(&me.conn, &bus) else {
                return;
            };
            if let Ok(mut players) = me.players.lock() {
                if let Some(p) = players.iter_mut().find(|p| p.bus == bus) {
                    p.position_us = pos;
                    p.position_at = Some(Instant::now());
                }
            }
            me.changed();
        });
    }

    fn call<B>(&self, bus: &str, interface: &'static str, method: &'static str, body: B)
    where
        B: serde::Serialize + zbus::zvariant::DynamicType + Send + 'static,
    {
        let (conn, bus) = (self.conn.clone(), bus.to_string());
        std::thread::spawn(move || {
            let called = zbus::blocking::Proxy::new(&conn, bus.clone(), PATH, interface).and_then(|p| p.call::<_, _, ()>(method, &body));
            match called {
                Ok(()) => tracing::info!("mpris: {method} em {bus}"),
                Err(err) => tracing::warn!("mpris: {method} em {bus} falhou: {err}"),
            }
        });
    }

    /// Lê (de novo) as propriedades do player `bus` e guarda. Devolve se ele existe.
    fn refresh(&self, bus: &str) {
        let Some(mut fresh) = read_player(&self.conn, bus) else {
            return;
        };
        let Ok(mut players) = self.players.lock() else {
            return;
        };
        let old = players.iter().position(|p| p.bus == bus);
        let was_playing = old.is_some_and(|i| players[i].status == Status::Playing);
        if fresh.status == Status::Playing && !was_playing {
            fresh.played = self.order.fetch_add(1, Ordering::Relaxed) + 1;
        } else if let Some(i) = old {
            fresh.played = players[i].played;
        }
        // A posição lida agora vale; sem ela (player que não diz), fica a de antes.
        if fresh.position_at.is_none() {
            if let Some(i) = old {
                fresh.position_us = players[i].position_us;
                fresh.position_at = players[i].position_at;
            }
        }
        match old {
            Some(i) if players[i] == fresh => return,
            Some(i) => players[i] = fresh,
            None => {
                tracing::info!("mpris: player {} ({bus})", fresh.name());
                players.push(fresh);
            }
        }
        drop(players);
        self.changed();
    }

    fn forget(&self, bus: &str) {
        let removed = self.players.lock().map(|mut players| {
            let before = players.len();
            players.retain(|p| p.bus != bus);
            before != players.len()
        });
        if removed.unwrap_or(false) {
            tracing::info!("mpris: player {bus} saiu");
            self.changed();
        }
    }

    /// Players do dono único `owner` (os sinais vêm dele).
    fn buses_of(&self, owner: &str) -> Vec<String> {
        self.players.lock().map(|p| p.iter().filter(|p| p.owner == owner).map(|p| p.bus.clone()).collect()).unwrap_or_default()
    }
}

fn read_position(conn: &zbus::blocking::Connection, bus: &str) -> Option<i64> {
    let proxy = zbus::blocking::Proxy::new(conn, bus.to_string(), PATH, PLAYER).ok()?;
    let value: OwnedValue = proxy.get_property("Position").ok()?;
    number(&value)
}

fn read_player(conn: &zbus::blocking::Connection, bus: &str) -> Option<Player> {
    let props = zbus::blocking::fdo::PropertiesProxy::builder(conn).destination(bus.to_string()).ok()?.path(PATH).ok()?.build().ok()?;
    let player_iface = zbus::names::InterfaceName::try_from(PLAYER).ok()?;
    let player: HashMap<String, OwnedValue> = props.get_all(player_iface.into()).ok()?;
    let root_iface = zbus::names::InterfaceName::try_from(ROOT).ok()?;
    let root: HashMap<String, OwnedValue> = props.get_all(root_iface.into()).unwrap_or_default();
    let get = |map: &HashMap<String, OwnedValue>, key: &str| map.get(key).map(|v| &**v).and_then(text).unwrap_or_default();
    let flag = |key: &str, default: bool| player.get(key).and_then(|v| boolean(v)).unwrap_or(default);
    let meta: HashMap<String, OwnedValue> = player
        .get("Metadata")
        .and_then(|v| match inner(v) {
            Value::Dict(dict) => Some(dict.iter().filter_map(|(k, v)| Some((text(k)?, OwnedValue::try_from(v.try_clone().ok()?).ok()?))).collect()),
            _ => None,
        })
        .unwrap_or_default();
    let owner = zbus::blocking::fdo::DBusProxy::new(conn)
        .ok()
        .and_then(|d| d.get_name_owner(zbus::names::BusName::try_from(bus).ok()?).ok())
        .map(|o| o.to_string())
        .unwrap_or_else(|| bus.to_string());
    // `Position` não vem em todo `GetAll` (alguns players dão erro nela); lida à parte.
    let position = player.get("Position").and_then(|v| number(v)).or_else(|| read_position(conn, bus));
    Some(Player {
        bus: bus.to_string(),
        owner,
        identity: get(&root, "Identity"),
        desktop_entry: get(&root, "DesktopEntry"),
        status: Status::parse(&get(&player, "PlaybackStatus")),
        track: parse_metadata(&meta),
        can_next: flag("CanGoNext", false),
        can_previous: flag("CanGoPrevious", false),
        can_play: flag("CanPlay", true),
        can_pause: flag("CanPause", true),
        can_seek: flag("CanSeek", false),
        can_control: flag("CanControl", true),
        can_raise: root.get("CanRaise").and_then(|v| boolean(v)).unwrap_or(false),
        volume: player.get("Volume").and_then(|v| float(v)).filter(|v| *v >= 0.0).map(|v| v.min(1.0)),
        rate: player.get("Rate").and_then(|v| float(v)).filter(|r| *r > 0.0).unwrap_or(1.0),
        position_us: position.unwrap_or(0),
        position_at: position.map(|_| Instant::now()),
        played: 0,
    })
}

/// Começa a acompanhar os players do barramento da sessão. Sem barramento, `None`.
pub fn start() -> Option<Mpris> {
    let conn = zbus::blocking::connection::Builder::session().ok()?.method_timeout(std::time::Duration::from_millis(800)).build().ok()?;
    let mpris = Mpris {
        players: Arc::default(),
        chosen: Arc::default(),
        order: Arc::default(),
        version: Arc::default(),
        conn: conn.clone(),
    };
    // Sinais primeiro (para não perder um player que aparece durante a lista inicial).
    let watch = mpris.clone();
    std::thread::spawn(move || {
        let rule = zbus::MatchRule::builder()
            .msg_type(zbus::message::Type::Signal)
            .sender("org.freedesktop.DBus")
            .and_then(|b| b.member("NameOwnerChanged"))
            .and_then(|b| b.arg0ns("org.mpris.MediaPlayer2"))
            .map(|b| b.build());
        let Ok(rule) = rule else {
            return;
        };
        let Ok(messages) = zbus::blocking::MessageIterator::for_match_rule(rule, &watch.conn, Some(64)) else {
            return;
        };
        for message in messages.flatten() {
            if let Ok((name, _old, new)) = message.body().deserialize::<(String, String, String)>() {
                if !name.starts_with(PREFIX) {
                    continue;
                }
                if new.is_empty() {
                    watch.forget(&name);
                } else {
                    watch.refresh(&name);
                }
            }
        }
    });
    let watch = mpris.clone();
    std::thread::spawn(move || {
        let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).path(PATH).map(|b| b.build());
        let Ok(rule) = rule else {
            return;
        };
        let Ok(messages) = zbus::blocking::MessageIterator::for_match_rule(rule, &watch.conn, Some(64)) else {
            return;
        };
        for message in messages.flatten() {
            let header = message.header();
            let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
            let member = header.member().map(|m| m.to_string()).unwrap_or_default();
            let buses = watch.buses_of(&sender);
            match member.as_str() {
                "Seeked" => {
                    if let Ok(pos) = message.body().deserialize::<i64>() {
                        if let Ok(mut players) = watch.players.lock() {
                            for p in players.iter_mut().filter(|p| buses.contains(&p.bus)) {
                                p.position_us = pos;
                                p.position_at = Some(Instant::now());
                            }
                        }
                        watch.changed();
                    }
                }
                "PropertiesChanged" => {
                    for bus in buses {
                        watch.refresh(&bus);
                    }
                }
                _ => {}
            }
        }
    });
    let dbus = zbus::blocking::fdo::DBusProxy::new(&conn).ok()?;
    for name in dbus.list_names().ok()? {
        let name = name.to_string();
        if name.starts_with(PREFIX) {
            mpris.refresh(&name);
        }
    }
    tracing::info!("mpris: acompanhando os players");
    Some(mpris)
}

/// Capa da música: um arquivo local pronto para abrir, ou ainda baixando / sem capa.
#[derive(Debug, Clone, PartialEq)]
pub enum Art {
    File(PathBuf),
    Pending,
    Missing,
}

/// Pasta das capas baixadas (`~/.cache/mide/nowplaying`).
fn art_dir() -> Option<PathBuf> {
    let base = std::env::var_os("XDG_CACHE_HOME").map(PathBuf::from).or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".cache")))?;
    Some(base.join("mide").join("nowplaying"))
}

/// Nome do arquivo da capa de `url` no cache (FNV-1a de 64 bits: estável entre
/// versões do Rust, ao contrário do `DefaultHasher`).
pub fn art_cache_name(url: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for b in url.as_bytes() {
        hash ^= *b as u64;
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{hash:016x}.img")
}

impl Mpris {
    /// Capa de `url`. Arquivos locais (`file://`) valem na hora; endereços da internet
    /// são baixados uma vez pelo `curl` (programa externo, como a Calculadora faz com as
    /// cotações) numa thread e guardados no cache; quando o download acaba, `version`
    /// muda. Nos testes (`MIDE_NO_NETWORK`), só com o curl falso de `MIDE_CURL`.
    pub fn art(&self, url: &str) -> Art {
        static STATE: Mutex<Option<HashMap<String, Art>>> = Mutex::new(None);
        if let Some(path) = url.strip_prefix("file://") {
            let path = PathBuf::from(percent_decode(path));
            return if path.is_file() { Art::File(path) } else { Art::Missing };
        }
        if !(url.starts_with("https://") || url.starts_with("http://")) {
            return Art::Missing;
        }
        let Some(dir) = art_dir() else {
            return Art::Missing;
        };
        let file = dir.join(art_cache_name(url));
        if std::fs::metadata(&file).is_ok_and(|m| m.len() > 0) {
            return Art::File(file);
        }
        let Ok(mut state) = STATE.lock() else {
            return Art::Missing;
        };
        let state = state.get_or_insert_with(HashMap::new);
        if let Some(known) = state.get(url) {
            return known.clone();
        }
        let curl = match std::env::var_os("MIDE_CURL") {
            Some(curl) => PathBuf::from(curl),
            None if std::env::var_os("MIDE_NO_NETWORK").is_some() => {
                tracing::info!("mpris: capa (teste): sem MIDE_CURL, nada baixado");
                state.insert(url.to_string(), Art::Missing);
                return Art::Missing;
            }
            None => PathBuf::from("curl"),
        };
        state.insert(url.to_string(), Art::Pending);
        let (me, url) = (self.clone(), url.to_string());
        std::thread::spawn(move || {
            let _ = std::fs::create_dir_all(&dir);
            trim_cache(&dir, 60);
            let part = file.with_extension("part");
            let ok = std::process::Command::new(&curl)
                .args(["-sfL", "--max-time", "15", "--max-filesize", "8000000", "-o"])
                .arg(&part)
                .arg(&url)
                .status()
                .is_ok_and(|s| s.success())
                && std::fs::metadata(&part).is_ok_and(|m| m.len() > 0)
                && std::fs::rename(&part, &file).is_ok();
            let _ = std::fs::remove_file(&part);
            tracing::info!("mpris: capa {}", if ok { "baixada" } else { "não baixada" });
            if let Ok(mut state) = STATE.lock() {
                state.get_or_insert_with(HashMap::new).insert(url, if ok { Art::File(file) } else { Art::Missing });
            }
            me.changed();
        });
        Art::Pending
    }
}

/// Deixa só as `keep` capas mais novas no cache.
fn trim_cache(dir: &std::path::Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> =
        entries.flatten().filter_map(|e| Some((e.metadata().ok()?.modified().ok()?, e.path()))).collect();
    if files.len() <= keep {
        return;
    }
    files.sort();
    for (_, path) in &files[..files.len() - keep] {
        let _ = std::fs::remove_file(path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn owned(v: Value<'static>) -> OwnedValue {
        OwnedValue::try_from(v).unwrap()
    }

    #[test]
    fn metadados_do_spotify() {
        let mut meta = HashMap::new();
        meta.insert("mpris:trackid".to_string(), owned(Value::ObjectPath(zbus::zvariant::ObjectPath::try_from("/com/spotify/track/abc").unwrap())));
        meta.insert("xesam:title".to_string(), owned(Value::from("Aquarela")));
        meta.insert("xesam:artist".to_string(), owned(Value::from(vec!["Toquinho", "Vinicius"])));
        meta.insert("xesam:album".to_string(), owned(Value::from("Aquarela")));
        meta.insert("mpris:artUrl".to_string(), owned(Value::from("https://i.scdn.co/image/ab67")));
        meta.insert("mpris:length".to_string(), owned(Value::U64(245_000_000)));
        let track = parse_metadata(&meta);
        assert_eq!(track.id, "/com/spotify/track/abc");
        assert_eq!(track.title, "Aquarela");
        assert_eq!(track.artist, "Toquinho, Vinicius");
        assert_eq!(track.album, "Aquarela");
        assert_eq!(track.art_url, "https://i.scdn.co/image/ab67");
        assert_eq!(track.length_us, Some(245_000_000));
    }

    #[test]
    fn metadados_com_variantes_e_tipos_diferentes() {
        // Variante dentro de variante, artista como texto solto, duração como `i32` e
        // como número quebrado (players que fogem do padrão).
        let mut meta = HashMap::new();
        meta.insert("xesam:title".to_string(), owned(Value::Value(Box::new(Value::from("Faixa")))));
        meta.insert("xesam:artist".to_string(), owned(Value::from("Só um")));
        meta.insert("mpris:length".to_string(), owned(Value::I32(1_000_000)));
        let track = parse_metadata(&meta);
        assert_eq!(track.title, "Faixa");
        assert_eq!(track.artist, "Só um");
        assert_eq!(track.length_us, Some(1_000_000));
        meta.insert("mpris:length".to_string(), owned(Value::F64(2.5e6)));
        assert_eq!(parse_metadata(&meta).length_us, Some(2_500_000));
        // Duração zero ou negativa: sem duração.
        meta.insert("mpris:length".to_string(), owned(Value::I64(0)));
        assert_eq!(parse_metadata(&meta).length_us, None);
    }

    #[test]
    fn titulo_de_reserva_pelo_arquivo() {
        let track = Track { url: "file:///home/a/M%C3%BAsicas/minha%20faixa.mp3".into(), ..Default::default() };
        assert_eq!(track.display_title(), "minha faixa.mp3");
        let track = Track { title: "  Nome  ".into(), url: "file:///x.mp3".into(), ..Default::default() };
        assert_eq!(track.display_title(), "Nome");
    }

    fn player(bus: &str, status: Status, played: u64) -> Player {
        Player { bus: bus.into(), status, played, ..Default::default() }
    }

    #[test]
    fn principal_e_o_ultimo_que_tocou() {
        assert_eq!(choose(&[], None), None);
        // Nenhum tocou ainda: o que está tocando, depois o pausado, depois o primeiro.
        let ps = [player("a", Status::Stopped, 0), player("b", Status::Paused, 0), player("c", Status::Playing, 0)];
        assert_eq!(choose(&ps, None), Some(2));
        let ps = [player("a", Status::Stopped, 0), player("b", Status::Paused, 0)];
        assert_eq!(choose(&ps, None), Some(1));
        let ps = [player("a", Status::Stopped, 0), player("b", Status::Stopped, 0)];
        assert_eq!(choose(&ps, None), Some(0));
        // O último que começou a tocar ganha, mesmo pausado depois.
        let ps = [player("spotify", Status::Paused, 5), player("firefox", Status::Playing, 3)];
        assert_eq!(choose(&ps, None), Some(0));
    }

    #[test]
    fn escolha_do_usuario_vale_ate_outro_tocar() {
        let ps = [player("spotify", Status::Playing, 5), player("vlc", Status::Paused, 2)];
        // Escolhido depois (ordem 6): o VLC passa a ser o principal.
        let chosen = ("vlc".to_string(), 6);
        assert_eq!(choose(&ps, Some(&chosen)), Some(1));
        // O Spotify começa a tocar de novo (ordem 7): volta a ser ele.
        let ps = [player("spotify", Status::Playing, 7), player("vlc", Status::Paused, 2)];
        assert_eq!(choose(&ps, Some(&chosen)), Some(0));
        // Escolha de um player que saiu: ignorada.
        let gone = ("rhythmbox".to_string(), 99);
        assert_eq!(choose(&ps, Some(&gone)), Some(0));
    }

    #[test]
    fn tempo_como_no_mac() {
        assert_eq!(format_time(0), "0:00");
        assert_eq!(format_time(187_400_000), "3:07");
        assert_eq!(format_time(3_723_000_000), "1:02:03");
        assert_eq!(format_time(-5), "0:00");
    }

    #[test]
    fn posicao_estimada_respeita_a_duracao() {
        let mut p = player("a", Status::Paused, 0);
        p.position_us = 10;
        p.track.length_us = Some(5);
        assert_eq!(p.position_now(), 5);
        p.track.length_us = None;
        p.position_us = -3;
        assert_eq!(p.position_now(), 0);
    }

    #[test]
    fn nome_do_player() {
        let p = Player { bus: "org.mpris.MediaPlayer2.vlc.instance123".into(), ..Default::default() };
        assert_eq!(p.name(), "Vlc");
        let p = Player { bus: "org.mpris.MediaPlayer2.spotify".into(), identity: "Spotify".into(), ..Default::default() };
        assert_eq!(p.name(), "Spotify");
    }

    #[test]
    fn nome_da_capa_no_cache_e_estavel() {
        assert_eq!(art_cache_name("https://a/b"), art_cache_name("https://a/b"));
        assert_ne!(art_cache_name("https://a/b"), art_cache_name("https://a/c"));
        assert!(art_cache_name("x").ends_with(".img"));
    }
}
