//! Barra superior do mide, como a barra de menus do macOS.
//!
//! Cliente Wayland com janela de camada no topo da tela e zona exclusiva (as janelas
//! não ficam por baixo dela).
//!
//! - Esquerda: a marca do mide (abre o menu do sistema) e o nome do app em foco.
//! - Direita: bateria (se houver) e relógio com data.
//!
//! O menu do sistema é uma segunda camada (overlay) em tela cheia e transparente: o
//! painel do menu fica embaixo da marca, e um clique fora dele fecha o menu.

use std::{
    collections::HashMap,
    error::Error,
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::PathBuf,
    process::Command,
    time::Duration,
};

use chrono::{Datelike, Local, Timelike};
use mide::{
    calendar,
    system::{self, Status},
    ui::{
        draw::{fill_rounded, rgba, rounded_rect, to_argb8888, vertical_gradient},
        text::{FontWeight, Text},
    },
};
use smithay_client_toolkit::{
    compositor::{CompositorHandler, CompositorState},
    delegate_registry,
    output::{OutputHandler, OutputState},
    reexports::{
        calloop::{
            channel,
            timer::{TimeoutAction, Timer},
            EventLoop,
        },
        calloop_wayland_source::WaylandSource,
        client::{
            globals::registry_queue_init,
            protocol::{wl_output, wl_pointer, wl_seat, wl_shm, wl_surface},
            Connection, QueueHandle,
        },
    },
    registry::{ProvidesRegistryState, RegistryState},
    registry_handlers,
    seat::{
        pointer::{PointerEvent, PointerEventKind, PointerHandler},
        Capability, SeatHandler, SeatState,
    },
    shell::{
        wlr_layer::{
            Anchor, KeyboardInteractivity, Layer, LayerShell, LayerShellHandler, LayerSurface,
            LayerSurfaceConfigure,
        },
        WaylandSurface,
    },
    shm::{slot::SlotPool, Shm, ShmHandler},
};
use tiny_skia::{FillRule, Paint, PathBuilder, Pixmap, Stroke, Transform};

const BAR_HEIGHT: u32 = 28;
const FONT: f32 = 13.5;
const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

/// Área clicável da marca do mide, à esquerda.
const LOGO_X: f32 = 14.0;
const LOGO_W: f32 = 22.0;

/// Painéis dos menus.
const MENU_MIN_W: f32 = 220.0;
const ITEM_H: f32 = 24.0;
const SEPARATOR_H: f32 = 9.0;
const MENU_PAD: f32 = 5.0;
/// Espaço entre os títulos dos menus na barra.
const TITLE_GAP: f32 = 18.0;

/// Ação de um item de menu.
#[derive(Debug, Clone, PartialEq, Eq)]
enum MenuAction {
    About,
    /// Abre outra janela do app em foco (pelo `Exec` do `.desktop`).
    NewWindow,
    /// Comando do canal de controle do compositor (`window close`, `keys ctrl+c`...).
    Compositor(String),
    /// Comando para todas as janelas do app em foco (`hide`, `close`).
    AppWindows(&'static str),
    /// Abre um painel das configurações do sistema.
    Settings(&'static str),
    /// Repouso e bloquear tela (sem confirmação).
    Power(system::Power),
    /// Pede confirmação (reiniciar, desligar, encerrar a sessão).
    Confirm(MenuId),
    /// Fecha à força o app em foco (quando ele não responde).
    ForceQuit,
    /// Item do menu do próprio app (menu global): o número dele no menu do app.
    Native(i32),
    /// Item do menu de um ícone do tray: o ícone e o número do item.
    Tray(u8, i32),
}

/// Itens de um menu `dbusmenu` (menu global ou de um ícone do tray). Submenus aparecem
/// logo abaixo do título deles, com recuo.
fn dbus_entries(items: &[mide::appmenu::Item], action: &dyn Fn(i32) -> MenuAction) -> Vec<MenuItem> {
    fn push(out: &mut Vec<MenuItem>, items: &[mide::appmenu::Item], depth: usize, action: &dyn Fn(i32) -> MenuAction) {
        for item in items {
            if item.separator {
                if !matches!(out.last(), Some(MenuItem::Separator) | None) {
                    out.push(MenuItem::Separator);
                }
                continue;
            }
            let mark = match item.checked {
                Some(true) => "✓ ",
                _ => "",
            };
            let label = format!("{}{mark}{}", "    ".repeat(depth), item.label);
            if item.children.is_empty() {
                out.push(entry(label, &item.shortcut, item.enabled.then(|| action(item.id))));
            } else {
                out.push(entry(label, "", None));
                push(out, &item.children, depth + 1, action);
            }
        }
    }
    let mut out = Vec::new();
    push(&mut out, items, 0, action);
    out
}

/// Chave da imagem de um ícone do tray (muda quando o ícone muda).
fn tray_key(item: &mide::tray::TrayItem) -> String {
    let pixels = item.pixmap.as_ref().map(|(w, h, d)| format!("{w}x{h}:{}", d.iter().map(|b| *b as u64).sum::<u64>())).unwrap_or_default();
    format!("{}{}|{}|{}|{pixels}", item.bus, item.path, item.icon_name, item.theme_path)
}

/// Imagem de um ícone do tray em `size` px: a que o app mandou (ARGB32, big-endian) ou
/// o ícone pelo nome (pasta do app, depois o tema).
fn tray_icon(item: &mide::tray::TrayItem, size: u32) -> Option<Pixmap> {
    if let Some((w, h, data)) = &item.pixmap {
        let mut image = Pixmap::new(*w as u32, *h as u32)?;
        for (dst, src) in image.pixels_mut().iter_mut().zip(data.chunks_exact(4)) {
            let (a, r, g, b) = (src[0], src[1], src[2], src[3]);
            let k = |c: u8| (c as u32 * a as u32 / 255) as u8;
            *dst = tiny_skia::PremultipliedColorU8::from_rgba(k(r), k(g), k(b), a)?;
        }
        let mut out = Pixmap::new(size, size)?;
        let s = size as f32 / (*w).max(*h) as f32;
        out.draw_pixmap(0, 0, image.as_ref(), &tiny_skia::PixmapPaint { quality: tiny_skia::FilterQuality::Bicubic, ..Default::default() }, Transform::from_scale(s, s), None);
        return Some(out);
    }
    if item.icon_name.is_empty() {
        return None;
    }
    if !item.theme_path.is_empty() {
        for ext in ["png", "svg"] {
            let file = std::path::Path::new(&item.theme_path).join(format!("{}.{ext}", item.icon_name));
            if file.exists() {
                if let Some(icon) = mide::icons::load_icon_file(&file.to_string_lossy(), size) {
                    return Some(icon);
                }
            }
        }
    }
    mide::icons::load_icon(&item.icon_name, size)
}

/// Item de menu: texto, atalho mostrado à direita e ação (sem ação, desativado).
#[derive(Debug, Clone)]
enum MenuItem {
    Entry(String, String, Option<MenuAction>),
    Separator,
}

/// Topo dos menus e painéis: abaixo da barra, com espaço para a ponta que mostra de
/// onde eles vêm.
const PANEL_TOP: f32 = BAR_HEIGHT as f32 + mide::ui::draw::ARROW_H + 2.0;

fn entry(label: impl Into<String>, shortcut: &str, action: Option<MenuAction>) -> MenuItem {
    MenuItem::Entry(label.into(), shortcut.to_string(), action)
}

/// Qual menu: o do sistema (marca do mide) ou um dos menus do app em foco.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum MenuId {
    System,
    App,
    File,
    Edit,
    Window,
    /// Menu do próprio app (menu global), pela posição na barra de menus dele.
    Native(u8),
    /// Ícone de um app no tray (pela posição na lista de ícones).
    Tray(u8),
    /// Painéis da direita (desenhados à parte, não são listas).
    Calendar,
    Sound,
    Wifi,
    Bluetooth,
    /// Menu da bateria (lista).
    Battery,
    /// Seletor do layout de teclado (lista), com mais de um layout nas Configurações.
    Layout,
    /// Gravação de tela em andamento: o clique para (não é menu).
    Record,
    /// Central de controle (Wi-Fi, Bluetooth, Não perturbe, brilho, som).
    Control,
    /// Engrenagem: abre as Configurações (não é menu).
    Gear,
    /// Tocando Agora (B29): o player de música ou vídeo, com capa e controles.
    NowPlaying,
    /// Confirmações do menu do mide.
    ConfirmRestart,
    ConfirmShutdown,
    ConfirmLogout,
}

impl MenuId {
    /// Painel desenhado à parte (calendário, som, Wi-Fi, Bluetooth).
    fn custom(self) -> bool {
        matches!(
            self,
            MenuId::Calendar
                | MenuId::Sound
                | MenuId::Wifi
                | MenuId::Bluetooth
                | MenuId::Control
                | MenuId::NowPlaying
                | MenuId::ConfirmRestart
                | MenuId::ConfirmShutdown
                | MenuId::ConfirmLogout
        )
    }

    fn confirm(self) -> bool {
        matches!(self, MenuId::ConfirmRestart | MenuId::ConfirmShutdown | MenuId::ConfirmLogout)
    }

    /// Largura do painel desenhado à parte.
    fn custom_width(self) -> f32 {
        match self {
            MenuId::Wifi | MenuId::ConfirmRestart | MenuId::ConfirmShutdown | MenuId::ConfirmLogout => 300.0,
            MenuId::Control | MenuId::NowPlaying => 320.0,
            _ => 280.0,
        }
    }
}

/// O que um clique num painel da direita faz.
#[derive(Debug, Clone, PartialEq)]
enum PanelAction {
    PrevMonth,
    NextMonth,
    Today,
    /// Trilho do volume: o valor vem da posição do clique.
    VolumeTrack,
    ToggleMute,
    ToggleWifi,
    Connect(String),
    ToggleBluetooth,
    Device(usize),
    Settings(&'static str),
    /// Central de controle: Não perturbe e o trilho do brilho.
    ToggleDnd,
    BrightnessTrack,
    /// Confirmação: fazer ou cancelar.
    ConfirmYes,
    ConfirmNo,
    /// Calendário: abrir/fechar as opções do relógio e ligar uma parte dele.
    ClockOptions,
    ClockPart(usize),
    /// Central de notificações: apagar o histórico.
    ClearNotifications,
    /// Painel de som: tocar por esta saída / gravar por esta entrada (índice na lista).
    AudioOutput(usize),
    AudioInput(usize),
    /// Central de notificações: aviso clicado (índice no histórico).
    Notification(usize),
    /// Tocando Agora: escolher o player (índice na lista), anterior, tocar/pausar,
    /// próxima, o trilho da posição, o trilho do volume do player e abrir o app.
    NpPlayer(usize),
    NpPrevious,
    NpPlayPause,
    NpNext,
    NpSeek,
    NpVolume,
    NpRaise,
}

/// Títulos dos menus do app, depois do nome dele.
const APP_MENUS: &[(MenuId, &str)] = &[(MenuId::File, "Arquivo"), (MenuId::Edit, "Editar"), (MenuId::Window, "Janela")];

/// Itens de cada menu. Os menus do app valem para qualquer app: as ações são feitas
/// pelo compositor (janela, atalhos de teclado), como a barra do macOS.
fn menu_items(id: MenuId, app: &str) -> Vec<MenuItem> {
    let keys = |combo: &str| Some(MenuAction::Compositor(format!("keys {combo}")));
    let window = |what: &str| Some(MenuAction::Compositor(format!("window {what}")));
    match id {
        MenuId::System => vec![
            entry("Sobre este computador", "", Some(MenuAction::About)),
            MenuItem::Separator,
            entry("Configurações do Sistema…", "", Some(MenuAction::Settings(""))),
            MenuItem::Separator,
            entry("Forçar encerrar o app", "", Some(MenuAction::ForceQuit)),
            MenuItem::Separator,
            entry("Repouso", "", Some(MenuAction::Power(system::Power::Sleep))),
            entry("Reiniciar…", "", Some(MenuAction::Confirm(MenuId::ConfirmRestart))),
            entry("Desligar…", "", Some(MenuAction::Confirm(MenuId::ConfirmShutdown))),
            MenuItem::Separator,
            entry("Bloquear tela", "", Some(MenuAction::Power(system::Power::Lock))),
            entry("Encerrar sessão…", "", Some(MenuAction::Confirm(MenuId::ConfirmLogout))),
        ],
        MenuId::App => vec![
            entry(format!("Ocultar {app}"), "", Some(MenuAction::AppWindows("hide"))),
            MenuItem::Separator,
            entry(format!("Encerrar {app}"), "", Some(MenuAction::AppWindows("close"))),
        ],
        MenuId::File => vec![
            entry("Nova janela", "", Some(MenuAction::NewWindow)),
            MenuItem::Separator,
            entry("Fechar janela", "Ctrl+Shift+Q", window("close")),
        ],
        MenuId::Edit => vec![
            entry("Desfazer", "Ctrl+Z", keys("ctrl+z")),
            entry("Refazer", "Ctrl+Shift+Z", keys("ctrl+shift+z")),
            MenuItem::Separator,
            entry("Recortar", "Ctrl+X", keys("ctrl+x")),
            entry("Copiar", "Ctrl+C", keys("ctrl+c")),
            entry("Colar", "Ctrl+V", keys("ctrl+v")),
            entry("Selecionar tudo", "Ctrl+A", keys("ctrl+a")),
        ],
        // Como o menu Janela do Mac (o mesmo do `window_menu` do mide-ui); os submenus
        // aparecem com recuo, como os dos apps. As telas e as janelas do app entram em
        // `Bar::window_extras`.
        MenuId::Window => vec![
            entry("Minimizar", "Super+H", window("minimize")),
            entry("Preencher", "", window("fill")),
            entry("Centralizar", "", window("center")),
            MenuItem::Separator,
            entry("Mover e redimensionar", "", None),
            entry("    Metade esquerda", "", window("left")),
            entry("    Metade direita", "", window("right")),
            entry("    Metade de cima", "", window("top")),
            entry("    Metade de baixo", "", window("bottom")),
            entry("    Canto superior esquerdo", "", window("top-left")),
            entry("    Canto superior direito", "", window("top-right")),
            entry("    Canto inferior esquerdo", "", window("bottom-left")),
            entry("    Canto inferior direito", "", window("bottom-right")),
            entry("    Voltar ao tamanho anterior", "", window("restore")),
            entry("Lado a lado na tela", "", None),
            entry("    À esquerda da tela", "", window("left")),
            entry("    À direita da tela", "", window("right")),
            MenuItem::Separator,
            entry("Minimizar todas", "", window("minimize-all")),
            entry("Trazer todas para a frente", "", window("front")),
        ],
        MenuId::Battery => vec![entry("Configurações de bateria…", "", Some(MenuAction::Settings("energy")))],
        // Itens vêm dos layouts escolhidos (`Bar::layout_items`).
        MenuId::Layout | MenuId::Record => Vec::new(),
        // Itens vêm do app (`Bar::native_items`).
        MenuId::Native(_) | MenuId::Tray(_) => Vec::new(),
        // Painéis desenhados à parte (e a engrenagem, que não é menu): sem itens de lista.
        MenuId::Calendar | MenuId::Sound | MenuId::Wifi | MenuId::Bluetooth | MenuId::Control | MenuId::Gear | MenuId::NowPlaying => Vec::new(),
        MenuId::ConfirmRestart | MenuId::ConfirmShutdown | MenuId::ConfirmLogout => Vec::new(),
    }
}

fn main() -> Result<(), Box<dyn Error>> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| "mide_bar=info".into()),
        )
        .init();

    let conn = Connection::connect_to_env()?;
    let (globals, event_queue) = registry_queue_init(&conn)?;
    let qh = event_queue.handle();

    let compositor = CompositorState::bind(&globals, &qh)?;
    let layer_shell = LayerShell::bind(&globals, &qh)?;
    let shm = Shm::bind(&globals, &qh)?;

    let surface = compositor.create_surface(&qh);
    let layer = layer_shell.create_layer_surface(&qh, surface, Layer::Top, Some("mide-bar"), None);
    layer.set_anchor(Anchor::TOP | Anchor::LEFT | Anchor::RIGHT);
    layer.set_size(0, BAR_HEIGHT);
    layer.set_exclusive_zone(BAR_HEIGHT as i32);
    layer.set_keyboard_interactivity(KeyboardInteractivity::None);
    layer.commit();

    let pool = SlotPool::new(1920 * BAR_HEIGHT as usize * 4, &shm)?;
    let mut bar = Bar {
        registry_state: RegistryState::new(&globals),
        seat_state: SeatState::new(&globals, &qh),
        output_state: OutputState::new(&globals, &qh),
        compositor,
        layer_shell,
        shm,
        pool,
        layer,
        width: 0,
        configured: false,
        exit: false,
        pointer: None,
        text: Text::new(),
        focused_name: String::new(),
        focused_id: String::new(),
        focused_pid: 0,
        dark_seen: mide::kit::system_dark(),
        titles: Vec::new(),
        over_launcher: false,
        launcher_k: 0.0,
        tone_dark: None,
        tone_k: if wallpaper_top_dark() { 1.0 } else { 0.0 },
        clock: String::new(),
        items: Vec::new(),
        appmenu: mide::appmenu::start_registrar(),
        tray: mide::tray::start(),
        tray_items: Vec::new(),
        tray_version: u64::MAX,
        tray_icons: HashMap::new(),
        mpris: mide::mpris::start(),
        mpris_version: u64::MAX,
        np: None,
        np_players: Vec::new(),
        np_images: HashMap::new(),
        np_logged: String::new(),
        np_scroll_from: std::time::Instant::now(),
        np_position_read: std::time::Instant::now(),
        np_second: -1,
        native: None,
        native_gtk: None,
        layout_active: None,
        recording: None,
        native_failed: None,
        native_failed_at: std::time::Instant::now(),
        titles_read_at: std::time::Instant::now(),
        registrar_checked_at: std::time::Instant::now(),
        battery: None,
        names: AppNames::load(),
        status: Status::default(),
        status_now: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true)),
        cal_month: (2000, 1),
        clock_options: false,
        history: Vec::new(),
        panel_room: 600.0,
        volume_sent: std::time::Instant::now(),
        menu: None,
        bar_dirty: false,
        menu_dirty: false,
        glass_bar: Default::default(),
        glass_menu: Default::default(),
        qh: qh.clone(),
    };

    let mut event_loop: EventLoop<Bar> = EventLoop::try_new()?;
    WaylandSource::new(conn.clone(), event_queue)
        .insert(event_loop.handle())
        .map_err(|e| e.to_string())?;
    // Som, Wi-Fi e Bluetooth: lidos numa thread (os comandos levam alguns ms) a cada
    // 2 s, ou na hora quando a barra pede (depois de uma ação ou ao abrir um painel).
    let (status_tx, status_rx) = channel::channel::<Status>();
    let now_flag = bar.status_now.clone();
    std::thread::spawn(move || {
        // Leitura anterior e a última troca automática (como o Mac: fone ligado vira a
        // saída; tirado, volta aos alto-falantes). Uma tentativa por aparelho a cada
        // 30 s, para não insistir se a troca não pegar.
        let mut before: Option<Status> = None;
        let mut tried: Vec<(String, std::time::Instant)> = Vec::new();
        loop {
            let status = system::read_status();
            let empty = Status::default();
            let prev = before.as_ref().unwrap_or(&empty);
            // "Padrão sumiu" só conta se continuar na leitura seguinte (a troca de perfil
            // tira o nó por um instante), e nada muda logo depois de uma escolha.
            let lists = [(true, &status.outputs, &prev.outputs, status.output_gone && prev.output_gone), (false, &status.inputs, &prev.inputs, status.input_gone && prev.input_gone)];
            for (output, now_list, prev_list, gone) in lists {
                if system::recently_selected() {
                    break;
                }
                if let Some(device) = mide::audio::follow(prev_list, now_list, gone) {
                    tried.retain(|(_, at)| at.elapsed() < Duration::from_secs(30));
                    if !tried.iter().any(|(k, _)| *k == device.key) {
                        tracing::info!("som: {} automática → {}", if output { "saída" } else { "entrada" }, device.name);
                        tried.push((device.key.clone(), std::time::Instant::now()));
                        system::select_audio(&device, output);
                    }
                }
            }
            before = Some(status.clone());
            if status_tx.send(status).is_err() {
                break;
            }
            for _ in 0..20 {
                if now_flag.swap(false, std::sync::atomic::Ordering::Relaxed) {
                    break;
                }
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    });
    event_loop
        .handle()
        .insert_source(status_rx, |event, _, bar: &mut Bar| {
            if let channel::Event::Msg(status) = event {
                if status != bar.status {
                    // Durante o arrasto do volume, o valor local manda.
                    let dragging = bar.menu.as_ref().is_some_and(|m| m.dragging_volume);
                    let volume = bar.status.volume;
                    if status.wired_primary() != bar.status.wired_primary() {
                        tracing::info!("rede: ícone da barra = {}", if status.wired_primary() { "cabo" } else { "Wi-Fi" });
                    }
                    bar.status = status;
                    if dragging {
                        bar.status.volume = volume;
                    }
                    bar.draw_bar();
                    if bar.menu.as_ref().is_some_and(|m| m.id.custom()) {
                        bar.draw_menu();
                    }
                }
            }
        })
        .map_err(|e| e.to_string())?;
    // Relógio, bateria e app em foco: atualiza a cada meio segundo e só redesenha se
    // mudar. O launcher é conferido mais vezes, para a barra mudar junto com ele.
    // Relógio adaptativo: 50 ms normalmente (o launcher é percebido logo) e 16 ms
    // durante a transição da barra ao abrir ou fechar o launcher.
    let mut last_refresh = std::time::Instant::now();
    event_loop
        .handle()
        .insert_source(Timer::from_duration(Duration::from_millis(50)), move |_, _, bar| {
            bar.refresh_launcher();
            bar.refresh_tray();
            let scrolling = bar.refresh_now_playing();
            if last_refresh.elapsed() >= Duration::from_millis(450) {
                bar.refresh();
                last_refresh = std::time::Instant::now();
            }
            let animating = bar.step_launcher_fade();
            let animating = bar.step_tone() || animating || scrolling;
            TimeoutAction::ToDuration(Duration::from_millis(if animating { 16 } else { 50 }))
        })
        .map_err(|e| e.to_string())?;

    tracing::info!("barra pronta");
    while !bar.exit {
        event_loop.dispatch(None, &mut bar)?;
        if bar.bar_dirty {
            bar.paint_bar();
        }
        if bar.menu_dirty {
            bar.paint_menu();
        }
    }
    Ok(())
}

/// Nomes de apps a partir dos `.desktop`, indexados pelos ids possíveis do app_id.
struct AppNames {
    /// Nome, comando (`Exec` sem códigos de campo) e ícone pelo app_id.
    by_id: HashMap<String, (String, String, String)>,
}

impl AppNames {
    fn load() -> Self {
        let locales = freedesktop_desktop_entry::get_languages_from_env();
        let mut by_id = HashMap::new();
        for entry in freedesktop_desktop_entry::desktop_entries(&locales) {
            let Some(name) = entry.name(&locales).map(|n| n.into_owned()) else {
                continue;
            };
            let exec = mide::apps::clean_exec(entry.exec().unwrap_or(""));
            let icon = entry.icon().unwrap_or("").to_string();
            let id = entry.id().to_string();
            let mut ids = vec![id.to_lowercase()];
            if let Some(last) = id.rsplit('.').next() {
                ids.push(last.to_lowercase());
            }
            if let Some(class) = entry.startup_wm_class() {
                ids.push(class.to_lowercase());
            }
            for key in ids {
                by_id.entry(key).or_insert_with(|| (name.clone(), exec.clone(), icon.clone()));
            }
        }
        Self { by_id }
    }

    fn name_for(&self, app_id: &str) -> Option<String> {
        self.by_id.get(&app_id.to_lowercase()).map(|(name, ..)| name.clone())
    }

    fn exec_for(&self, app_id: &str) -> Option<String> {
        self.by_id.get(&app_id.to_lowercase()).map(|(_, exec, _)| exec.clone()).filter(|e| !e.is_empty())
    }

    /// Ícone (`Icon=`) do app, se o `.desktop` tiver.
    fn icon_for(&self, app_id: &str) -> Option<String> {
        self.by_id.get(&app_id.to_lowercase()).map(|(_, _, icon)| icon.clone()).filter(|i| !i.is_empty())
    }
}

/// Envia um comando ao compositor pelo canal de controle e devolve as linhas.
fn ask_compositor(command: &str) -> Option<Vec<String>> {
    let runtime = std::env::var_os("XDG_RUNTIME_DIR")?;
    let display = std::env::var("WAYLAND_DISPLAY").ok()?;
    let path = PathBuf::from(runtime).join(format!("{display}.ctl"));
    let mut stream = UnixStream::connect(path).ok()?;
    stream.set_read_timeout(Some(Duration::from_millis(300))).ok()?;
    stream.write_all(format!("{command}\n").as_bytes()).ok()?;
    let mut lines = Vec::new();
    for line in BufReader::new(stream).lines() {
        let line = line.ok()?;
        if line == "end" {
            break;
        }
        lines.push(line);
    }
    Some(lines)
}

/// Data e hora no estilo do macOS em português: "sáb 17 out  20:15".
/// O que o relógio da barra mostra além da hora (`[clock]` no `config.toml`, escolhido
/// em "Formato da hora…" no calendário). Por padrão, só a hora.
const CLOCK_PARTS: &[(&str, &str)] = &[("weekday", "Dia da semana"), ("date", "Dia e mês"), ("year", "Ano"), ("seconds", "Segundos")];

fn clock_part(key: &str) -> bool {
    matches!(mide::settings_file::get("clock", key), Some(mide::settings_file::Value::Bool(true)))
}

fn clock_text() -> String {
    clock_format(Local::now(), clock_part("weekday"), clock_part("date"), clock_part("year"), clock_part("seconds"))
}

/// "qua 18 out 2026  14:05:09", com as partes escolhidas (sempre a hora).
fn clock_format(now: chrono::DateTime<Local>, weekday: bool, date: bool, year: bool, seconds: bool) -> String {
    const DAYS: [&str; 7] = ["seg", "ter", "qua", "qui", "sex", "sáb", "dom"];
    const MONTHS: [&str; 12] = ["jan", "fev", "mar", "abr", "mai", "jun", "jul", "ago", "set", "out", "nov", "dez"];
    let mut parts = Vec::new();
    if weekday {
        parts.push(DAYS[now.weekday().num_days_from_monday() as usize].to_string());
    }
    if date {
        parts.push(format!("{} {}", now.day(), MONTHS[now.month0() as usize]));
    }
    if year {
        parts.push(now.year().to_string());
    }
    let time = if seconds { format!("{:02}:{:02}:{:02}", now.hour(), now.minute(), now.second()) } else { format!("{:02}:{:02}", now.hour(), now.minute()) };
    if parts.is_empty() {
        time
    } else {
        format!("{}  {time}", parts.join(" "))
    }
}

/// Carga da bateria (0 a 100) e se está carregando, se o computador tiver bateria.
/// `MIDE_POWER_SUPPLY_DIR` troca a pasta do sistema (os testes usam uma vazia: a barra
/// fica igual com ou sem bateria na máquina que roda os testes).
fn battery() -> Option<(u8, bool)> {
    let root = std::env::var_os("MIDE_POWER_SUPPLY_DIR").unwrap_or_else(|| "/sys/class/power_supply".into());
    let dir = fs::read_dir(root).ok()?;
    for entry in dir.flatten() {
        let path = entry.path();
        if !path.file_name()?.to_string_lossy().starts_with("BAT") {
            continue;
        }
        let capacity = fs::read_to_string(path.join("capacity")).ok()?.trim().parse().ok()?;
        let status = fs::read_to_string(path.join("status")).unwrap_or_default();
        return Some((capacity, status.trim() == "Charging"));
    }
    None
}

/// Menu aberto: a camada em tela cheia (clique fora fecha), o menu mostrado, onde
/// fica o painel e o item sob o ponteiro.
struct Menu {
    layer: LayerSurface,
    width: u32,
    height: u32,
    id: MenuId,
    items: Vec<MenuItem>,
    x: f32,
    panel_w: f32,
    hovered: Option<usize>,
    /// Painéis da direita: áreas clicáveis do último desenho, a sob o ponteiro, a
    /// altura e se o volume está sendo arrastado.
    regions: Vec<((f32, f32, f32, f32), PanelAction)>,
    hover_region: Option<usize>,
    panel_h: f32,
    dragging_volume: bool,
    /// Arrastando o trilho do brilho (central de controle).
    dragging_brightness: bool,
    /// Tocando Agora: arrastando a posição ou o volume do player, e o valor (0 a 1).
    dragging_np: Option<(PanelAction, f32)>,
    /// Topo do painel na tela (abaixo da barra; as confirmações ficam no meio).
    panel_top: f32,
}

impl Menu {
    fn panel_height(&self) -> f32 {
        if self.id.custom() {
            return self.panel_h;
        }
        let items: f32 = self
            .items
            .iter()
            .map(|i| match i {
                MenuItem::Entry(..) => ITEM_H,
                MenuItem::Separator => SEPARATOR_H,
            })
            .sum();
        items + 2.0 * MENU_PAD
    }

    /// Índice do item sob `(x, y)`, se for um item com ação.
    fn item_at(&self, x: f64, y: f64) -> Option<usize> {
        let (x, y) = (x as f32, y as f32);
        let top = PANEL_TOP + MENU_PAD;
        if x < self.x || x > self.x + self.panel_w {
            return None;
        }
        let mut cursor = top;
        for (i, item) in self.items.iter().enumerate() {
            let h = match item {
                MenuItem::Entry(..) => ITEM_H,
                MenuItem::Separator => SEPARATOR_H,
            };
            if y >= cursor && y < cursor + h {
                return matches!(item, MenuItem::Entry(_, _, Some(_))).then_some(i);
            }
            cursor += h;
        }
        None
    }

    fn inside_panel(&self, x: f64, y: f64) -> bool {
        let (x, y) = (x as f32, y as f32);
        let top = self.panel_top;
        x >= self.x && x <= self.x + self.panel_w && y >= top && y <= top + self.panel_height()
    }
}

/// Cores dos menus e painéis da barra, no tema claro ou escuro (`[appearance]`),
/// lidas no começo de cada desenho (`refresh_palette`).
#[derive(Clone, Copy)]
struct Palette {
    dark: bool,
    /// O azul de destaque do tema (o mesmo dos apps do mide-ui).
    accent: [u8; 4],
    text: [u8; 4],
    secondary: [u8; 4],
    faint: [u8; 4],
    body: [u8; 4],
}

impl Palette {
    const LIGHT: Palette = Palette { dark: false, accent: [10, 110, 235, 255], text: [24, 24, 28, 255], secondary: [120, 120, 128, 255], faint: [165, 165, 172, 255], body: [70, 70, 78, 255] };
    const DARK: Palette = Palette { dark: true, accent: [30, 125, 245, 255], text: [236, 236, 240, 255], secondary: [160, 160, 168, 255], faint: [110, 110, 118, 255], body: [205, 205, 212, 255] };

    fn accent_color(&self) -> tiny_skia::Color {
        rgba(self.accent[0], self.accent[1], self.accent[2], 255)
    }

    /// Destaque apertado (um pouco mais escuro).
    fn accent_pressed(&self) -> tiny_skia::Color {
        let d = |c: u8| c.saturating_sub(25);
        rgba(d(self.accent[0]), d(self.accent[1]), d(self.accent[2]), 255)
    }

    /// Linhas, bordas e fundos de botão: preto no claro, branco no escuro.
    fn ink(&self, alpha: u8) -> tiny_skia::Color {
        if self.dark {
            rgba(255, 255, 255, alpha)
        } else {
            rgba(0, 0, 0, alpha)
        }
    }

    /// Fundo dos menus e painéis.
    fn panel(&self, alpha: u8) -> [u8; 4] {
        if self.dark {
            [40, 40, 44, alpha]
        } else {
            [246, 246, 248, alpha]
        }
    }

    /// Ladrilhos da central de controle.
    fn tile(&self) -> tiny_skia::Color {
        if self.dark {
            rgba(255, 255, 255, 22)
        } else {
            rgba(255, 255, 255, 150)
        }
    }

    fn bar_top(&self, alpha: u8) -> tiny_skia::Color {
        if self.dark {
            rgba(46, 46, 50, alpha)
        } else {
            rgba(250, 250, 252, alpha)
        }
    }

    fn bar_bottom(&self, alpha: u8) -> tiny_skia::Color {
        if self.dark {
            rgba(36, 36, 40, alpha)
        } else {
            rgba(238, 238, 242, alpha)
        }
    }
}

thread_local! {
    static PALETTE: std::cell::Cell<Palette> = const { std::cell::Cell::new(Palette::LIGHT) };
}

/// O alto do papel de parede é escuro? (Na barra sem fundo, o texto fica branco.) A
/// imagem é amostrada uma vez por arquivo; o gradiente padrão do mide é escuro em cima.
fn wallpaper_top_dark() -> bool {
    thread_local! {
        static CACHE: std::cell::RefCell<Option<(String, Option<std::time::SystemTime>, bool)>> = const { std::cell::RefCell::new(None) };
    }
    let path = match mide::settings_file::cached("desktop", "wallpaper") {
        Some(mide::settings_file::Value::Text(p)) if !p.trim().is_empty() => p,
        _ => return true,
    };
    let mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
    if let Some(dark) = CACHE.with(|c| c.borrow().as_ref().filter(|(p, m, _)| *p == path && *m == mtime).map(|(_, _, d)| *d)) {
        return dark;
    }
    let dark = image::open(&path)
        .map(|img| {
            let small = img.thumbnail(256, 256).to_rgb8();
            let rows = (small.height() / 25).max(1);
            let mut sum = 0u64;
            let mut n = 0u64;
            for y in 0..rows {
                for x in 0..small.width() {
                    let p = small.get_pixel(x, y);
                    sum += (p[0] as u64 * 299 + p[1] as u64 * 587 + p[2] as u64 * 114) / 1000;
                    n += 1;
                }
            }
            sum / n.max(1) < 145
        })
        .unwrap_or(true);
    tracing::info!("barra: papel de parede {} em cima", if dark { "escuro" } else { "claro" });
    CACHE.with(|c| *c.borrow_mut() = Some((path, mtime, dark)));
    dark
}

/// O desenho com uma sombra de 1 pixel embaixo (cor `shade`, pela transparência dele).
fn with_shadow(src: &Pixmap, shade: [u8; 4]) -> Pixmap {
    let mut out = Pixmap::new(src.width(), src.height()).expect("tamanho");
    let mut shadow = src.clone();
    for p in shadow.pixels_mut() {
        let a = (p.alpha() as u32 * shade[3] as u32 / 255) as u8;
        let pm = |c: u8| (c as u32 * a as u32 / 255) as u8;
        *p = tiny_skia::PremultipliedColorU8::from_rgba(pm(shade[0]), pm(shade[1]), pm(shade[2]), a).unwrap_or(tiny_skia::PremultipliedColorU8::TRANSPARENT);
    }
    out.draw_pixmap(0, 1, shadow.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), None);
    out.draw_pixmap(0, 0, src.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), None);
    out
}

fn pal() -> Palette {
    PALETTE.with(|p| p.get())
}

/// Lê o tema (claro/escuro) para os próximos desenhos.
fn refresh_palette() {
    let theme = mide::kit::Theme::system();
    let base = if theme.dark { Palette::DARK } else { Palette::LIGHT };
    PALETTE.with(|p| p.set(Palette { accent: theme.accent, ..base }));
}

struct Bar {
    /// Vidro atrás da barra e atrás do painel do menu aberto (desenhado pelo compositor).
    glass_bar: mide::control_client::Glass,
    glass_menu: mide::control_client::Glass,
    registry_state: RegistryState,
    seat_state: SeatState,
    output_state: OutputState,
    compositor: CompositorState,
    layer_shell: LayerShell,
    shm: Shm,
    pool: SlotPool,
    layer: LayerSurface,
    width: u32,
    configured: bool,
    exit: bool,
    pointer: Option<wl_pointer::WlPointer>,
    text: Text,
    focused_name: String,
    /// app_id da janela em foco (vazio sem janela em foco).
    focused_id: String,
    /// Processo da janela em foco (dos menus registrados, vale o dele).
    focused_pid: u32,
    /// Tema escuro da última conferência (a troca nas Configurações redesenha sozinha).
    dark_seen: bool,
    /// Títulos desenhados na barra: menu e faixa x (início, fim), para o clique.
    titles: Vec<(MenuId, f32, f32)>,
    /// Launcher aberto: barra transparente sobre ele, com texto branco.
    over_launcher: bool,
    /// Transição para o launcher: 0 barra normal, 1 sem fundo e com texto branco.
    launcher_k: f32,
    /// O compositor diz se o que está atrás da barra é escuro (`None`: não disse, vale
    /// o alto do papel de parede).
    tone_dark: Option<bool>,
    /// Transição do texto da barra sem fundo: 0 = texto escuro (fundo claro), 1 = texto
    /// branco (fundo escuro).
    tone_k: f32,
    clock: String,
    /// Quais itens de `BAR_ITEMS` estão à mostra (para redesenhar quando mudam).
    items: Vec<bool>,
    /// Menus dos apps (menu global): o registrar e a conexão, e o menu do app em foco
    /// (nome no barramento, objeto e os menus de cima).
    appmenu: Option<(mide::appmenu::Registry, zbus::blocking::Connection)>,
    /// Ícones dos apps (tray): o registro, os ícones à mostra, a versão vista e as
    /// imagens já preparadas (pela chave do ícone e a imagem dele).
    tray: Option<mide::tray::Tray>,
    tray_items: Vec<mide::tray::TrayItem>,
    tray_version: u64,
    tray_icons: HashMap<String, Option<Pixmap>>,
    /// Tocando Agora (B29): os players (MPRIS), a versão vista, o principal e a lista,
    /// e as imagens já preparadas (ícones dos apps e capas, pela chave).
    mpris: Option<mide::mpris::Mpris>,
    mpris_version: u64,
    np: Option<mide::mpris::Player>,
    np_players: Vec<mide::mpris::Player>,
    np_images: HashMap<String, Option<Pixmap>>,
    /// O que foi registrado no log por último (para registrar só mudanças).
    np_logged: String,
    /// Começo da rolagem do título na barra (troca de música recomeça).
    np_scroll_from: std::time::Instant,
    /// Último pedido da posição ao player (com o painel aberto, a cada poucos segundos).
    np_position_read: std::time::Instant,
    /// Segundo mostrado no painel (redesenha quando muda).
    np_second: i64,
    native: Option<(String, String, Vec<mide::appmenu::Item>)>,
    /// Layout de teclado ativo (número na lista das Configurações), quando o seletor
    /// aparece na barra.
    layout_active: Option<u32>,
    /// Gravação de tela em andamento: segundos desde o começo.
    recording: Option<u64>,
    /// O menu em foco é de um app GTK (`org.gtk.Menus`, avisado ao compositor pelo
    /// `gtk_shell1`): onde ele está e as ações dos itens (o `id` de cada item é o
    /// número da ação). Sem isso, o `native` é um `dbusmenu`.
    native_gtk: Option<(mide::gmenu::GtkMenu, Vec<mide::gmenu::Action>)>,
    /// Menu que não pôde ser lido (não tenta de novo a cada meio segundo).
    /// Menu que não deu para ler, e quando: tenta de novo depois de alguns segundos.
    native_failed: Option<(String, String)>,
    native_failed_at: std::time::Instant,
    /// Última releitura dos títulos do menu do app em foco.
    titles_read_at: std::time::Instant,
    /// Última conferência de que o registrar dos menus ainda é da barra.
    registrar_checked_at: std::time::Instant,
    battery: Option<(u8, bool)>,
    names: AppNames,
    /// Som, Wi-Fi e Bluetooth, lidos numa thread a cada 2 s.
    status: Status,
    /// Pede à thread uma leitura imediata (depois de uma ação).
    status_now: std::sync::Arc<std::sync::atomic::AtomicBool>,
    /// Mês mostrado no calendário (ano, mês).
    cal_month: (i32, u32),
    /// O calendário mostra as opções do formato da hora.
    clock_options: bool,
    /// Notificações recentes (central de notificações, embaixo do calendário).
    history: Vec<mide::notify::Entry>,
    /// Altura disponível para o painel aberto (da barra até embaixo da tela).
    panel_room: f32,
    /// Última vez que o volume foi mandado durante um arrasto.
    volume_sent: std::time::Instant,
    menu: Option<Menu>,
    bar_dirty: bool,
    menu_dirty: bool,
    qh: QueueHandle<Bar>,
}

impl Bar {
    /// Com o launcher aberto, a barra fica transparente e com texto branco.
    /// Um passo da transição da barra (fundo some e texto clareia sobre o launcher).
    /// Devolve se ainda está animando.
    fn step_launcher_fade(&mut self) -> bool {
        let target = if self.over_launcher { 1.0 } else { 0.0 };
        if (self.launcher_k - target).abs() < f32::EPSILON {
            return false;
        }
        let step = 16.0 / 250.0;
        self.launcher_k = if target > self.launcher_k { (self.launcher_k + step).min(1.0) } else { (self.launcher_k - step).max(0.0) };
        self.draw_bar();
        true
    }

    /// O fundo atrás da barra é escuro? O que o compositor mediu ou, sem a medida
    /// (compositor antigo), o alto do papel de parede.
    fn behind_dark(&self) -> bool {
        self.tone_dark.unwrap_or_else(wallpaper_top_dark)
    }

    /// Um passo da troca de cor do texto da barra sem fundo (claro ↔ escuro), com a
    /// duração das transições curtas; sem "Transições suaves", troca de uma vez.
    /// Devolve se ainda está animando.
    fn step_tone(&mut self) -> bool {
        let target = if self.behind_dark() { 1.0 } else { 0.0 };
        if (self.tone_k - target).abs() < f32::EPSILON {
            return false;
        }
        let step = if mide::settings_file::effect("transitions") { 16.0 / mide::motion::VIEW_SWITCH.as_millis() as f32 } else { 1.0 };
        self.tone_k = if target > self.tone_k { (self.tone_k + step).min(1.0) } else { (self.tone_k - step).max(0.0) };
        // Só a barra sem fundo muda de cor; com fundo, a cor vem do tema.
        if !mide::settings_file::flag("bar", "background", false) {
            self.draw_bar();
        }
        (self.tone_k - target).abs() >= f32::EPSILON
    }

    fn refresh_launcher(&mut self) {
        let reply = ask_compositor("launcher state").unwrap_or_default();
        let open = reply.first().is_some_and(|l| l == "open");
        // Segunda linha: o tom atrás da barra (`tone dark|light`), medido pelo
        // compositor na hora em que o que está atrás muda.
        let tone = reply.iter().find_map(|l| l.strip_prefix("tone ")).map(|t| t == "dark");
        if tone.is_some() && tone != self.tone_dark {
            tracing::info!("barra: fundo atrás {}", if tone == Some(true) { "escuro (texto branco)" } else { "claro (texto escuro)" });
            self.tone_dark = tone;
        }
        if open != self.over_launcher {
            self.over_launcher = open;
            if open {
                self.close_menu();
            }
            self.draw_bar();
        }
    }

    /// Menu do próprio app em foco, se ele registrou um (menu global). Lido quando o app
    /// em foco muda ou quando o registro aparece depois.
    fn refresh_native(&mut self) {
        let Some((registry, conn)) = &self.appmenu else {
            return;
        };
        // Pelo registrar (Qt no X11, apps com dbusmenu); senão, o que o app disse ao
        // compositor pelo Wayland (GTK pelo `gtk_shell1`, Qt pelo menu do KDE).
        let mut gtk = None;
        let wanted = registry.menu_for_app(&self.focused_id, self.focused_pid).or_else(|| {
            let line = ask_compositor("appmenu")?.into_iter().next()?;
            let fields: Vec<&str> = line.split('\t').collect();
            match fields.as_slice() {
                ["gtk", bus, menubar, app, window] => {
                    gtk = Some(mide::gmenu::GtkMenu { bus: bus.to_string(), menubar: menubar.to_string(), app: app.to_string(), window: window.to_string() });
                    Some((bus.to_string(), menubar.to_string()))
                }
                ["dbusmenu", service, path] => Some((service.to_string(), path.to_string())),
                _ => None,
            }
        });
        let same = match (&wanted, &self.native) {
            (Some((s, p)), Some((ns, np, _))) => s == ns && p == np,
            (None, None) => true,
            _ => false,
        };
        let recent_failure = self.native_failed_at.elapsed() < std::time::Duration::from_secs(3);
        if same || (wanted.is_some() && wanted == self.native_failed && recent_failure) {
            return;
        }
        let previous_failure = self.native_failed.take();
        self.native_gtk = None;
        let failed = &mut self.native_failed;
        let native_gtk = &mut self.native_gtk;
        self.native = wanted.and_then(|(sender, path)| {
            let layout = match &gtk {
                Some(menu) => mide::gmenu::read(conn, menu).map(|(items, actions)| {
                    *native_gtk = Some((menu.clone(), actions));
                    items
                }),
                None => mide::appmenu::layout(conn, &sender, &path, 0),
            };
            let Some(layout) = layout else {
                // A mesma falha de novo (nova tentativa) não enche o log.
                if previous_failure.as_ref() != Some(&(sender.clone(), path.clone())) {
                    tracing::info!("menu do app {}: não foi possível ler", self.focused_id);
                }
                *failed = Some((sender, path));
                return None;
            };
            let top: Vec<mide::appmenu::Item> = layout.into_iter().filter(|i| !i.separator && !i.label.is_empty()).collect();
            tracing::info!("menu do app {}: {}", self.focused_id, top.iter().map(|i| i.label.as_str()).collect::<Vec<_>>().join(", "));
            (!top.is_empty()).then_some((sender, path, top))
        });
        if self.native_failed.is_some() {
            self.native_failed_at = std::time::Instant::now();
            // A conexão com o barramento caiu: todo menu falharia. Conecta de novo,
            // guardando os registros (os apps continuam com os mesmos nomes).
            if let Some((registry, conn)) = &self.appmenu {
                if !mide::appmenu::connection_alive(conn) || !mide::appmenu::owns_registrar(conn) {
                    tracing::warn!("menus dos apps: a conexão caiu ou o nome do registrar foi tomado; conectando de novo");
                    let registry = registry.clone();
                    self.appmenu = None;
                    self.appmenu = mide::appmenu::start_registrar_with(registry);
                    self.native_failed = None;
                }
            }
        }
        self.draw_bar();
    }

    /// Partes do menu Janela que mudam: "Mover para" as outras telas e a lista das
    /// janelas do app em foco (✓ na atual; clique traz a janela), como no Mac.
    fn window_extras(&self) -> Vec<MenuItem> {
        let mut out = Vec::new();
        let outputs: Vec<(String, String)> = ask_compositor("window-outputs")
            .unwrap_or_default()
            .iter()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (f.len() >= 3 && f[2] == "other").then(|| (f[0].to_string(), f[1].to_string()))
            })
            .collect();
        if !outputs.is_empty() {
            out.push(MenuItem::Separator);
            for (name, desc) in outputs {
                let label = if desc.trim().is_empty() { name.clone() } else { desc };
                out.push(entry(format!("Mover para {label}"), "", Some(MenuAction::Compositor(format!("window output={name}")))));
            }
        }
        let wins: Vec<(String, String, bool)> = ask_compositor("windows")
            .unwrap_or_default()
            .iter()
            .filter_map(|l| {
                let f: Vec<&str> = l.split('\t').collect();
                (f.len() >= 4 && !self.focused_id.is_empty() && f[1].eq_ignore_ascii_case(&self.focused_id)).then(|| (f[0].to_string(), f[2].to_string(), f[3] == "focused"))
            })
            .collect();
        if !wins.is_empty() {
            out.push(MenuItem::Separator);
            for (id, title, focused) in wins {
                let title = if title.chars().count() > 48 { format!("{}…", title.chars().take(47).collect::<String>()) } else { title };
                let label = if focused { format!("✓ {title}") } else { format!("    {title}") };
                out.push(entry(label, "", Some(MenuAction::Compositor(format!("focus {id}")))));
            }
        }
        out
    }

    /// Itens de um menu do app (lidos de novo ao abrir: o app pode ter mudado o menu).
    /// Submenus aparecem logo abaixo do título deles, com recuo.
    fn native_items(&mut self, index: usize) -> Vec<MenuItem> {
        let (Some((_, conn)), Some((sender, path, top))) = (&self.appmenu, &mut self.native) else {
            return Vec::new();
        };
        if let Some((menu, actions)) = &mut self.native_gtk {
            // GTK: lê a barra inteira de novo (ações ligadas e marcadas mudam).
            if let Some((items, new_actions)) = mide::gmenu::read(conn, menu) {
                *top = items.into_iter().filter(|i| !i.separator && !i.label.is_empty()).collect();
                *actions = new_actions;
            }
            return top.get(index).map(|m| dbus_entries(&m.children, &MenuAction::Native)).unwrap_or_default();
        }
        // Os números dos itens mudam quando o app troca os menus (a Calculadora ao trocar
        // de modo): a raiz é lida de novo e o menu é achado pelo título.
        let title = top.get(index).map(|m| m.label.clone());
        if let Some(root) = mide::appmenu::layout(conn, sender, path, 0) {
            let fresh: Vec<mide::appmenu::Item> = root.into_iter().filter(|i| !i.separator && !i.label.is_empty()).collect();
            if !fresh.is_empty() {
                *top = fresh;
            }
        }
        let menu = title.and_then(|t| top.iter().find(|m| m.label == t)).or_else(|| top.get(index));
        let Some(menu) = menu else {
            return Vec::new();
        };
        let children = mide::appmenu::layout(conn, sender, path, menu.id).filter(|c| !c.is_empty()).unwrap_or_else(|| menu.children.clone());
        dbus_entries(&children, &MenuAction::Native)
    }

    /// Relê os títulos dos menus do app em foco (o app pode ter trocado os menus sem
    /// trocar de janela). Só dbusmenu; os do GTK são lidos ao abrir.
    fn reread_native_titles(&mut self) {
        // Uma vez a cada 1,5 s (cada leitura é uma chamada ao app).
        if self.native_gtk.is_some() || self.titles_read_at.elapsed() < std::time::Duration::from_millis(1500) {
            return;
        }
        self.titles_read_at = std::time::Instant::now();
        let (Some((_, conn)), Some((sender, path, top))) = (&self.appmenu, &mut self.native) else {
            return;
        };
        let Some(root) = mide::appmenu::layout(conn, sender, path, 0) else {
            return;
        };
        let fresh: Vec<mide::appmenu::Item> = root.into_iter().filter(|i| !i.separator && !i.label.is_empty()).collect();
        let same = fresh.len() == top.len() && fresh.iter().zip(top.iter()).all(|(a, b)| a.label == b.label && a.id == b.id);
        if !fresh.is_empty() && !same {
            tracing::info!("menu do app {}: {}", self.focused_id, fresh.iter().map(|i| i.label.as_str()).collect::<Vec<_>>().join(", "));
            *top = fresh;
            self.draw_bar();
        }
    }

    /// Seletor de layout: os layouts escolhidos (o ativo marcado) e as Configurações.
    fn layout_items(&self) -> Vec<MenuItem> {
        let mut items: Vec<MenuItem> = mide::keyboard_layouts::configured()
            .iter()
            .enumerate()
            .map(|(i, c)| {
                let mark = if self.layout_active == Some(i as u32) { "✓ " } else { "    " };
                entry(format!("{mark}{}", c.name()), "", Some(MenuAction::Compositor(format!("layout {i}"))))
            })
            .collect();
        items.push(MenuItem::Separator);
        items.push(entry("Configurações de teclado…", "", Some(MenuAction::Settings("keyboard"))));
        items
    }

    /// Itens do menu de um ícone do tray.
    fn tray_menu_items(&self, index: usize) -> Vec<MenuItem> {
        let (Some(tray), Some(item)) = (&self.tray, self.tray_items.get(index)) else {
            return Vec::new();
        };
        let children = tray.menu(item).unwrap_or_default();
        let mut out = vec![entry(if item.title.is_empty() { item.id.clone() } else { item.title.clone() }, "", None), MenuItem::Separator];
        out.extend(dbus_entries(&children, &|id| MenuAction::Tray(index as u8, id)));
        if children.is_empty() {
            out.truncate(1);
        }
        out
    }

    /// Ícones do tray mudaram (um app entrou, saiu ou trocou o ícone): redesenha.
    fn refresh_tray(&mut self) {
        let Some(tray) = &self.tray else {
            return;
        };
        let version = tray.version();
        if version == self.tray_version {
            return;
        }
        self.tray_version = version;
        self.tray_items = tray.items();
        let keys: Vec<String> = self.tray_items.iter().map(tray_key).collect();
        self.tray_icons.retain(|k, _| keys.contains(k));
        self.draw_bar();
    }

    /// Atualiza relógio, bateria e app em foco; redesenha só se algo mudou.
    fn refresh(&mut self) {
        // Pedido do compositor: o botão de ligar abre a confirmação de desligar.
        if ask_compositor("bar_request").and_then(|r| r.into_iter().next()).as_deref() == Some("confirm-shutdown") {
            tracing::info!("botão de ligar: confirmação de desligar");
            self.open_menu(MenuId::ConfirmShutdown);
        }
        // Claro/escuro trocado nas Configurações: barra e menu aberto mudam sem esperar o
        // ponteiro passar por cima.
        // Também a cor de destaque (Configurações → Aparência).
        let dark = mide::kit::system_dark();
        let accent = mide::kit::system_accent(dark);
        if dark != self.dark_seen || accent != pal().accent {
            self.dark_seen = dark;
            tracing::info!("barra: tema {}", if dark { "escuro" } else { "claro" });
            refresh_palette();
            self.draw_bar();
            if self.menu.is_some() {
                self.draw_menu();
            }
        }
        let clock = clock_text();
        let battery = battery();
        let (focused_id, focused, focused_pid) = ask_compositor("list")
            .and_then(|lines| {
                lines.into_iter().find_map(|line| {
                    let mut parts = line.split('\t');
                    let (app_id, title, status) = (parts.next()?, parts.next()?, parts.next()?);
                    let pid = parts.next().and_then(|p| p.trim().parse().ok()).unwrap_or(0);
                    (status == "focused").then(|| {
                        let name = self
                            .names
                            .name_for(app_id)
                            .or_else(|| (!title.is_empty()).then(|| title.to_string()))
                            .unwrap_or_else(|| app_id.to_string());
                        (app_id.to_string(), name, pid)
                    })
                })
            })
            .unwrap_or_else(|| (String::new(), "mide".to_string(), 0));
        let items: Vec<bool> = mide::settings_file::BAR_ITEMS.iter().map(|(key, ..)| mide::settings_file::bar_item(key)).collect();
        if items != self.items {
            // Um item sumiu da barra: o painel dele (se aberto) fecha junto.
            if !self.items.is_empty() {
                tracing::info!("itens da barra: {}", mide::settings_file::BAR_ITEMS.iter().zip(&items).filter(|(_, on)| **on).map(|((k, ..), _)| *k).collect::<Vec<_>>().join(" "));
                self.close_menu();
            }
            self.items = items;
            self.draw_bar();
        }
        // Layout de teclado ativo (só com o seletor ligado: mais de um layout).
        let layout_active = if mide::keyboard_layouts::show_in_bar() {
            ask_compositor("layout").and_then(|l| l.into_iter().next()).and_then(|l| l.trim().parse().ok())
        } else {
            None
        };
        if layout_active != self.layout_active {
            self.layout_active = layout_active;
            self.draw_bar();
        }
        // Gravação de tela (⌘⇧5): o tempo na barra, com o botão de parar.
        let recording = ask_compositor("record status")
            .and_then(|l| l.into_iter().next())
            .and_then(|l| l.strip_prefix("on ").and_then(|r| r.split_whitespace().next()?.parse().ok()));
        if recording != self.recording {
            self.recording = recording;
            self.draw_bar();
        }
        let focus_changed = focused_id != self.focused_id || focused_pid != self.focused_pid;
        self.focused_pid = focused_pid;
        // Bateria fraca: ao passar de 10% descarregando, som e aviso (uma vez).
        if let (Some((was, _)), Some((now, false))) = (self.battery, battery) {
            if was > 10 && now <= 10 {
                mide::sounds::play("battery-low");
                mide::notify::send("Bateria", "Bateria fraca", &format!("{now}% restantes. Ligue o carregador."));
            }
        }
        if clock != self.clock || battery != self.battery || focused != self.focused_name || focused_id != self.focused_id {
            self.clock = clock;
            self.battery = battery;
            self.focused_name = focused;
            self.focused_id = focused_id;
            self.draw_bar();
        }
        // O menu do app em foco: ao trocar de app, e enquanto ele não registrou (o
        // registro pode chegar depois da janela).
        // O nome do registrar é da barra? Outro programa (outra barra, um teste) pode tê-lo
        // tomado; quando ele sai, a barra pega o nome de volta e os apps se registram de novo.
        if self.registrar_checked_at.elapsed() >= std::time::Duration::from_secs(5) {
            self.registrar_checked_at = std::time::Instant::now();
            let lost = self.appmenu.as_ref().is_none_or(|(_, conn)| !mide::appmenu::owns_registrar(conn));
            if lost {
                let registry = self.appmenu.take().map(|(r, _)| r).unwrap_or_default();
                self.appmenu = mide::appmenu::start_registrar_with(registry);
                if self.appmenu.as_ref().is_some_and(|(_, c)| mide::appmenu::owns_registrar(c)) {
                    tracing::info!("menus dos apps: registrar recuperado");
                    self.native = None;
                    self.native_failed = None;
                }
            }
        }
        if focus_changed || self.native.is_none() {
            self.refresh_native();
        } else if self.menu.is_none() {
            self.reread_native_titles();
        }
    }

    /// Pede um quadro novo da barra; o desenho sai uma vez depois dos eventos pendentes.
    fn draw_bar(&mut self) {
        self.bar_dirty = true;
    }

    /// Pede um quadro novo do menu aberto (mesmo agrupamento da barra).
    fn draw_menu(&mut self) {
        refresh_palette();
        self.menu_dirty = true;
    }

    fn paint_bar(&mut self) {
        refresh_palette();
        // Tahoe: sem fundo (texto direto sobre o papel de parede); `[bar] background`
        // traz o fundo translúcido de volta.
        let no_bg = !mide::settings_file::flag("bar", "background", false);
        // Tom do que está atrás da barra (medido pelo compositor), com transição: 0 =
        // fundo claro (texto escuro), 1 = fundo escuro (texto branco). Em 8 níveis,
        // como a cor do texto do launcher (o cache de texto guarda poucas cores).
        let tone = if no_bg { (mide::motion::smooth(self.tone_k) * 8.0).round() / 8.0 } else { 0.0 };
        let wall_dark = no_bg && tone >= 0.5;
        self.bar_dirty = false;
        if !self.configured || self.width == 0 {
            return;
        }
        let (w, h) = (self.width, BAR_HEIGHT);
        let Some(mut pixmap) = Pixmap::new(w, h) else {
            return;
        };
        let (fw, fh) = (w as f32, h as f32);

        // Fundo claro e translúcido, com uma linha fina embaixo, como no macOS. Com o
        // launcher aberto, sem fundo: só o texto branco sobre o launcher.
        // Com o launcher aberto, o fundo some e o texto fica branco, numa transição
        // suave (`launcher_k`), junto com o esmaecer do launcher.
        let k = self.launcher_k;
        let ease = mide::motion::smooth(k);
        // Cor do texto em 8 níveis: o cache de texto (por cor) guarda poucas versões.
        let level = (ease * 8.0).round() / 8.0;
        let mix = |a: u8, b: u8| (a as f32 + (b as f32 - a as f32) * level).round() as u8;
        let base = if no_bg {
            (18.0 + (250.0 - 18.0) * tone).round() as u8
        } else if pal().dark {
            245
        } else {
            20
        };
        let fg = [mix(base, 255), mix(base, 255), mix(base + 4, 255), mix(255, 250)];
        // Com o vidro ligado, o compositor desfoca o que está atrás da barra e o véu
        // claro fica mais leve (o texto escuro continua legível).
        let glass = mide::control_client::panel_glass() && ease < 0.5;
        let strip = [(0.0, 0.0, fw, fh, 0.0)];
        if !no_bg {
            self.glass_bar.set("mide-bar", if glass { &strip } else { &[] });
        }
        // Cápsula do item aberto: vidro claro sobre o papel de parede (sem fundo) ou o
        // destaque de sempre.
        let cap = if no_bg {
            rgba(255, 255, 255, (150.0 - (150.0 - 52.0) * tone).round() as u8)
        } else {
            pal().ink(30)
        };
        if ease < 1.0 && !no_bg {
            let (top_a, bottom_a) = if mide::control_client::panel_glass() { (158.0, 150.0) } else { (225.0, 220.0) };
            let fade = 1.0 - ease;
            let (top_a, bottom_a) = ((top_a * fade) as u8, (bottom_a * fade) as u8);
            let mut paint = Paint::default();
            paint.shader = vertical_gradient(0.0, fh, pal().bar_top(top_a), pal().bar_bottom(bottom_a));
            if let Some(rect) = tiny_skia::Rect::from_xywh(0.0, 0.0, fw, fh) {
                pixmap.fill_rect(rect, &paint, Transform::identity(), None);
            }
            if let Some(line) = tiny_skia::Rect::from_xywh(0.0, fh - 1.0, fw, 1.0) {
                let mut border = Paint::default();
                border.set_color(rgba(0, 0, 0, (34.0 * fade) as u8));
                pixmap.fill_rect(line, &border, Transform::identity(), None);
            }
        }

        // Marca do mide: um anel com um ponto, destacado quando o menu está aberto.
        let cy = fh / 2.0;
        let lx = LOGO_X + LOGO_W / 2.0;
        if self.menu.as_ref().is_some_and(|m| m.id == MenuId::System) {
            fill_rounded(&mut pixmap, LOGO_X - 4.0, 3.0, LOGO_W + 8.0, fh - 6.0, 5.0, cap);
        }
        if let Some(ring) = PathBuilder::from_circle(lx, cy, 6.5) {
            let mut stroke_paint = Paint::default();
            stroke_paint.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
            stroke_paint.anti_alias = true;
            let stroke = Stroke {
                width: 2.2,
                ..Default::default()
            };
            pixmap.stroke_path(&ring, &stroke_paint, &stroke, Transform::identity(), None);
        }
        if let Some(dot) = PathBuilder::from_circle(lx + 2.0, cy - 2.0, 2.2) {
            let mut dot_paint = Paint::default();
            dot_paint.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
            dot_paint.anti_alias = true;
            pixmap.fill_path(&dot, &dot_paint, FillRule::Winding, Transform::identity(), None);
        }

        // Nome do app em foco, em negrito, e os menus dele. O título do menu aberto
        // fica destacado.
        let text_y = (fh - FONT * 1.25) / 2.0;
        let open = self.menu.as_ref().map(|m| m.id);
        self.titles.clear();
        let mut tx = LOGO_X + LOGO_W + 14.0;
        let name = self.focused_name.clone();
        let mut titles: Vec<(MenuId, String, FontWeight)> = vec![(MenuId::App, name, FontWeight::Bold)];
        if let Some((_, _, top)) = &self.native {
            // Menus do próprio app (menu global).
            titles.extend(top.iter().enumerate().take(u8::MAX as usize).map(|(i, item)| (MenuId::Native(i as u8), item.label.clone(), FontWeight::Regular)));
        } else if !self.focused_id.is_empty() {
            titles.extend(APP_MENUS.iter().map(|(id, t)| (*id, t.to_string(), FontWeight::Regular)));
        }
        for (id, title, weight) in titles {
            let tw = self.text.measure(&title, FONT, weight);
            if open == Some(id) {
                fill_rounded(&mut pixmap, tx - 7.0, 3.0, tw + 14.0, fh - 6.0, 5.0, cap);
            }
            self.text.draw(&mut pixmap, &title, tx, text_y, FONT, weight, fg);
            self.titles.push((id, tx - TITLE_GAP / 2.0, tx + tw + TITLE_GAP / 2.0));
            tx += tw + TITLE_GAP;
        }
        // Sem janela em foco, o nome "mide" não tem menu.
        if self.focused_id.is_empty() {
            self.titles.clear();
        }

        // Direita, da direita para a esquerda: relógio (abre o calendário), bateria,
        // Bluetooth, Wi-Fi e som. Cada um abre o seu painel.
        let highlight = |pixmap: &mut Pixmap, id: MenuId, x0: f32, x1: f32| {
            if open == Some(id) {
                fill_rounded(pixmap, x0, 3.0, x1 - x0, fh - 6.0, 5.0, cap);
            }
        };
        let clock = self.clock.clone();
        let clock_w = self.text.measure(&clock, FONT, FontWeight::Regular);
        let mut x = fw - 14.0 - clock_w;
        highlight(&mut pixmap, MenuId::Calendar, x - 7.0, x + clock_w + 7.0);
        self.text.draw(&mut pixmap, &clock, x, text_y, FONT, FontWeight::Regular, fg);
        self.titles.push((MenuId::Calendar, x - 7.0, x + clock_w + 7.0));
        // Itens escondidos nas Configurações (`[bar]`) ficam de fora; o relógio fica.
        let shown = |key: &str| mide::settings_file::bar_item(key);
        // Central de controle logo à esquerda do relógio, como no macOS.
        if shown("control") {
            let (x0, x1) = (x - 36.0, x - 8.0);
            highlight(&mut pixmap, MenuId::Control, x0, x1);
            draw_control_icon(&mut pixmap, (x0 + x1) / 2.0, cy, fg);
            self.titles.push((MenuId::Control, x0, x1));
            x = x0;
        }
        if let (Some((level, charging)), true) = (self.battery, shown("battery")) {
            let label = format!("{level}%");
            let label_w = self.text.measure(&label, FONT, FontWeight::Regular);
            let right = x - 7.0;
            x -= 18.0 + label_w;
            let left = x - 30.0 - 4.0;
            highlight(&mut pixmap, MenuId::Battery, left, right);
            self.text.draw(&mut pixmap, &label, x, text_y, FONT, FontWeight::Regular, fg);
            x -= 30.0;
            draw_battery(&mut pixmap, x, cy, level, charging, fg);
            self.titles.push((MenuId::Battery, left, right));
        }
        // Gravando a tela: botão de parar (quadrado num círculo) e o tempo, como no Mac.
        if let Some(secs) = self.recording {
            let label = format!("{:02}:{:02}", secs / 60, secs % 60);
            let lw = self.text.measure(&label, FONT, FontWeight::Regular);
            let right = x - 7.0;
            let left = right - lw - 34.0;
            highlight(&mut pixmap, MenuId::Record, left, right);
            let (cx, cyy) = (left + 14.0, cy);
            if let Some(ring) = PathBuilder::from_circle(cx, cyy, 7.5) {
                let mut paint = Paint::default();
                paint.anti_alias = true;
                paint.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
                pixmap.stroke_path(&ring, &paint, &Stroke { width: 1.4, ..Default::default() }, Transform::identity(), None);
            }
            fill_rounded(&mut pixmap, cx - 3.0, cyy - 3.0, 6.0, 6.0, 1.0, rgba(fg[0], fg[1], fg[2], fg[3]));
            self.text.draw(&mut pixmap, &label, left + 27.0, text_y, FONT, FontWeight::Regular, fg);
            self.titles.push((MenuId::Record, left, right));
            x = left + 7.0;
        }
        // Layout de teclado (com mais de um nas Configurações): a sigla num contorno,
        // como no macOS.
        if let Some(active) = self.layout_active {
            let layouts = mide::keyboard_layouts::configured();
            if let Some(choice) = layouts.get(active as usize) {
                let label = choice.short();
                let lw = self.text.measure(&label, FONT - 1.5, FontWeight::Semibold);
                let right = x - 7.0;
                let left = right - lw - 22.0;
                highlight(&mut pixmap, MenuId::Layout, left, right);
                if let Some(path) = rounded_rect(left + 4.5, 6.5, right - left - 9.0, fh - 13.0, 4.0) {
                    let mut stroke = Paint::default();
                    stroke.anti_alias = true;
                    stroke.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
                    pixmap.stroke_path(&path, &stroke, &Stroke { width: 1.2, ..Default::default() }, Transform::identity(), None);
                }
                self.text.draw(&mut pixmap, &label, left + 11.0, text_y + 0.5, FONT - 1.5, FontWeight::Semibold, fg);
                self.titles.push((MenuId::Layout, left, right));
                x = left + 7.0;
            }
        }
        let icons: Vec<MenuId> = [
            (self.status.bluetooth.is_some() && shown("bluetooth"), MenuId::Bluetooth),
            ((self.status.wifi.is_some() || self.status.wired_connected().is_some()) && shown("wifi"), MenuId::Wifi),
            (self.status.volume.is_some() && shown("sound"), MenuId::Sound),
        ]
        .into_iter()
        .filter_map(|(present, id)| present.then_some(id))
        .collect();
        for id in icons {
            let (x0, x1) = (x - 36.0, x - 8.0);
            highlight(&mut pixmap, id, x0, x1);
            let cx = (x0 + x1) / 2.0;
            match id {
                MenuId::Bluetooth => draw_bluetooth_icon(&mut pixmap, cx, cy, self.status.bluetooth.as_ref().is_some_and(|(on, _)| *on), fg),
                MenuId::Wifi if self.status.wired_primary() => draw_ethernet_icon(&mut pixmap, cx, cy, fg),
                MenuId::Wifi => {
                    let level = match (&self.status.wifi, self.status.active_network()) {
                        (Some((false, _)), _) | (None, _) => None,
                        (_, None) => Some(0),
                        (_, Some(n)) => Some(1 + (n.signal as u32 * 3 / 100).min(2) as u8),
                    };
                    draw_wifi_icon(&mut pixmap, cx, cy, level, fg);
                }
                _ => draw_speaker_icon(&mut pixmap, cx, cy, self.status.volume, fg),
            }
            self.titles.push((id, x0, x1));
            x = x0;
        }
        // Engrenagem (abre as Configurações), à esquerda dos outros.
        if shown("gear") {
            let (x0, x1) = (x - 36.0, x - 8.0);
            highlight(&mut pixmap, MenuId::Gear, x0, x1);
            draw_gear_icon(&mut pixmap, (x0 + x1) / 2.0, cy, fg);
            self.titles.push((MenuId::Gear, x0, x1));
            x = x0;
        }
        // Ícones dos apps (tray), à esquerda dos do sistema, como no macOS.
        let tray_items = self.tray_items.clone();
        for (i, item) in tray_items.iter().enumerate().take(12) {
            let (x0, x1) = (x - 32.0, x - 8.0);
            highlight(&mut pixmap, MenuId::Tray(i as u8), x0, x1);
            let key = tray_key(item);
            let icon = self.tray_icons.entry(key).or_insert_with(|| tray_icon(item, 18)).clone();
            match icon {
                Some(icon) => pixmap.draw_pixmap(((x0 + x1) / 2.0 - 9.0) as i32, (cy - 9.0) as i32, icon.as_ref(), &tiny_skia::PixmapPaint::default(), Transform::identity(), None),
                // Sem imagem: a inicial do app.
                None => {
                    let letter: String = item.id.chars().next().map(|c| c.to_uppercase().collect()).unwrap_or_else(|| "?".into());
                    self.text.draw(&mut pixmap, &letter, (x0 + x1) / 2.0 - 4.0, text_y, FONT, FontWeight::Semibold, fg);
                }
            }
            self.titles.push((MenuId::Tray(i as u8), x0, x1));
            x = x0;
        }

        if no_bg {
            // Sombra leve atrás do texto e dos ícones (legíveis em qualquer papel de parede).
            pixmap = with_shadow(&pixmap, if wall_dark { [0, 0, 0, 90] } else { [255, 255, 255, 110] });
            // Vidro só atrás da cápsula do item aberto.
            let glass_on = mide::control_client::panel_glass();
            let open_rect: Vec<(f32, f32, f32, f32, f32)> = match open {
                Some(MenuId::System) => vec![(LOGO_X - 4.0, 3.0, LOGO_W + 8.0, fh - 6.0, 5.0)],
                // Menus da esquerda guardam o espaço entre eles; os da direita, a cápsula.
                Some(id) if matches!(id, MenuId::App | MenuId::Native(_) | MenuId::File | MenuId::Edit | MenuId::Window) => {
                    self.titles.iter().filter(|t| t.0 == id).map(|t| (t.1 + TITLE_GAP / 2.0 - 7.0, 3.0, t.2 - t.1 - TITLE_GAP + 14.0, fh - 6.0, 5.0)).collect()
                }
                Some(id) => self.titles.iter().filter(|t| t.0 == id).map(|t| (t.1, 3.0, t.2 - t.1, fh - 6.0, 5.0)).collect(),
                None => Vec::new(),
            };
            self.glass_bar.set("mide-bar", if glass_on { &open_rect } else { &[] });
        }
        self.present_layer(&pixmap, true);
    }

    fn paint_menu(&mut self) {
        self.menu_dirty = false;
        if self.menu.as_ref().is_some_and(|m| m.id.custom()) {
            self.draw_custom_panel();
            return;
        }
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let (w, h, hovered) = (menu.width, menu.height, menu.hovered);
        if w == 0 || h == 0 {
            return;
        }
        let Some(mut pixmap) = Pixmap::new(w, h) else {
            return;
        };
        let (mx, mw, items, menu_id) = (menu.x, menu.panel_w, menu.items.clone(), menu.id);
        let top = PANEL_TOP;
        let ph = menu.panel_height();

        // Painel: vidro claro com borda fina e sombra suave. Tahoe: menus com cantos de
        // 12 e a central de controle (e os painéis grandes) com 22.
        let radius = if menu_id == MenuId::Control { 22.0 } else { 12.0 };
        for i in 0..6 {
            let spread = 6.0 - i as f32;
            fill_rounded(&mut pixmap, mx - spread, top - spread + 3.0, mw + 2.0 * spread, ph + 2.0 * spread, radius + spread, rgba(0, 0, 0, 6));
        }
        let glass = mide::control_client::panel_glass();
        let panel = [(mx, top, mw, ph, radius)];
        self.glass_menu.set("mide-bar-menu", if glass { &panel } else { &[] });
        if glass {
            mide::ui::draw::clear_rounded(&mut pixmap, mx, top, mw, ph, radius);
        }
        let fill = pal().panel(if glass { 180 } else { 240 });
        fill_rounded(&mut pixmap, mx, top, mw, ph, radius, rgba(fill[0], fill[1], fill[2], fill[3]));
        if let Some(path) = rounded_rect(mx + 0.5, top + 0.5, mw - 1.0, ph - 1.0, radius - 0.5) {
            let mut border = Paint::default();
            border.set_color(pal().ink(40));
            border.anti_alias = true;
            pixmap.stroke_path(&path, &border, &Stroke::default(), Transform::identity(), None);
        }
        // Ponta apontando para o título (ou a marca do mide) de onde o menu veio.
        if let Some(cx) = self.anchor_of(menu_id) {
            mide::ui::draw::menu_arrow(&mut pixmap, cx.clamp(mx + 18.0, mx + mw - 18.0), top, false, fill);
        }

        let mut y = top + MENU_PAD;
        for (i, item) in items.iter().enumerate() {
            match item {
                MenuItem::Separator => {
                    if let Some(line) = tiny_skia::Rect::from_xywh(mx + 10.0, y + SEPARATOR_H / 2.0, mw - 20.0, 1.0) {
                        let mut paint = Paint::default();
                        paint.set_color(pal().ink(30));
                        pixmap.fill_rect(line, &paint, Transform::identity(), None);
                    }
                    y += SEPARATOR_H;
                }
                MenuItem::Entry(label, shortcut, action) => {
                    let active = hovered == Some(i);
                    if active {
                        fill_rounded(&mut pixmap, mx + 5.0, y, mw - 10.0, ITEM_H, 5.0, pal().accent_color());
                    }
                    let color = match (action.is_some(), active) {
                        (_, true) => [255, 255, 255, 255],
                        (true, false) => pal().text,
                        (false, false) => pal().faint,
                    };
                    let ty = y + (ITEM_H - FONT * 1.25) / 2.0;
                    self.text.draw(&mut pixmap, label, mx + 16.0, ty, FONT, FontWeight::Regular, color);
                    if !shortcut.is_empty() {
                        let sw = self.text.measure(shortcut, FONT - 1.0, FontWeight::Regular);
                        let sc = if active { [230, 236, 255, 255] } else { pal().secondary };
                        self.text.draw(&mut pixmap, shortcut, mx + mw - 16.0 - sw, ty + 0.5, FONT - 1.0, FontWeight::Regular, sc);
                    }
                    y += ITEM_H;
                }
            }
        }
        self.present_layer(&pixmap, false);
    }

    /// Envia o pixmap para a barra (`bar = true`) ou para o menu.
    fn present_layer(&mut self, pixmap: &Pixmap, bar: bool) {
        let (w, h) = (pixmap.width() as i32, pixmap.height() as i32);
        let Ok((buffer, canvas)) = self.pool.create_buffer(w, h, w * 4, wl_shm::Format::Argb8888) else {
            return;
        };
        to_argb8888(pixmap, canvas);
        let layer = if bar {
            &self.layer
        } else {
            match self.menu.as_ref() {
                Some(menu) => &menu.layer,
                None => return,
            }
        };
        let surface = layer.wl_surface();
        surface.damage_buffer(0, 0, w, h);
        if buffer.attach_to(surface).is_ok() {
            layer.commit();
        }
    }

    /// Abre o menu `id`, ou troca o menu aberto por ele (passar o mouse pelos títulos
    /// com um menu aberto troca de menu, como no macOS).
    fn open_menu(&mut self, id: MenuId) {
        // A engrenagem não é menu: abre as Configurações.
        if id == MenuId::Gear {
            self.close_menu();
            tracing::info!("engrenagem: Configurações");
            system::open_settings("");
            return;
        }
        // O botão da gravação também não: para a gravação.
        if id == MenuId::Record {
            self.close_menu();
            tracing::info!("barra: parar a gravação");
            ask_compositor("record stop");
            self.recording = None;
            self.draw_bar();
            return;
        }
        let app = self.focused_name.clone();
        // O menu Janela (o da barra ou o do app): ganha as outras telas e as janelas do app.
        let window_menu = match id {
            MenuId::Window => true,
            MenuId::Native(i) => self.native.as_ref().and_then(|(_, _, top)| top.get(i as usize)).is_some_and(|m| matches!(m.label.replace('_', "").as_str(), "Janela" | "Window")),
            _ => false,
        };
        let mut items = match id {
            MenuId::Native(i) => self.native_items(i as usize),
            MenuId::Tray(i) => self.tray_menu_items(i as usize),
            MenuId::Layout => self.layout_items(),
            _ => menu_items(id, &app),
        };
        if window_menu {
            items.extend(self.window_extras());
        }
        if id == MenuId::Battery {
            if let Some((level, charging)) = self.battery {
                let state = if charging { "carregando" } else { "na bateria" };
                items.insert(0, entry(format!("Bateria: {level}% ({state})"), "", None));
                items.insert(1, MenuItem::Separator);
            }
        }
        if id == MenuId::Calendar {
            let today = Local::now().date_naive();
            self.cal_month = (today.year(), today.month());
            self.clock_options = false;
            self.history = mide::notify::read_history();
        }
        let widest = items
            .iter()
            .map(|i| match i {
                MenuItem::Entry(label, shortcut, _) => {
                    let lw = self.text.measure(label, FONT, FontWeight::Regular);
                    let sw = if shortcut.is_empty() { 0.0 } else { self.text.measure(shortcut, FONT - 1.0, FontWeight::Regular) + 28.0 };
                    lw + sw + 34.0
                }
                MenuItem::Separator => 0.0,
            })
            .fold(MENU_MIN_W, f32::max);
        let widest = if id.custom() { id.custom_width() } else { widest };
        let title = self.titles.iter().find(|(t, _, _)| *t == id).map(|(_, x0, x1)| (*x0, *x1));
        let x = match (id, title) {
            (MenuId::System, _) => 8.0,
            // Confirmações: no meio da tela.
            (_, _) if id.confirm() => (self.width as f32 - id.custom_width()) / 2.0,
            // Painéis da direita: centrados no ícone, sem sair da tela.
            (_, Some((x0, x1))) if id.custom() || id == MenuId::Battery || id == MenuId::Layout => {
                (x0 + x1) / 2.0 - widest / 2.0
            }
            (_, Some((x0, _))) => x0 - 2.0,
            _ => 8.0,
        };
        // Nenhum menu passa da borda da tela (o de idiomas, mais largo, cortava).
        let x = x.min(self.width as f32 - widest - 8.0).max(8.0);

        tracing::info!("menu aberto: {id:?}");
        if id.custom() {
            self.request_status();
        }
        if let Some(menu) = self.menu.as_mut() {
            menu.id = id;
            menu.items = items;
            menu.x = x;
            menu.panel_w = widest;
            menu.hovered = None;
            menu.regions.clear();
            menu.hover_region = None;
            menu.dragging_volume = false;
            menu.dragging_brightness = false;
            menu.dragging_np = None;
            menu.panel_top = PANEL_TOP;
            self.draw_menu();
            self.draw_bar();
            return;
        }
        let surface = self.compositor.create_surface(&self.qh);
        let layer = self
            .layer_shell
            .create_layer_surface(&self.qh, surface, Layer::Overlay, Some("mide-bar-menu"), None);
        layer.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
        layer.set_size(0, 0);
        // Cobre a tela toda, inclusive a área da própria barra.
        layer.set_exclusive_zone(-1);
        layer.set_keyboard_interactivity(KeyboardInteractivity::None);
        layer.commit();
        self.menu = Some(Menu {
            layer,
            width: 0,
            height: 0,
            id,
            items,
            x,
            panel_w: widest,
            hovered: None,
            regions: Vec::new(),
            hover_region: None,
            panel_h: 0.0,
            panel_top: PANEL_TOP,
            dragging_volume: false,
            dragging_brightness: false,
            dragging_np: None,
        });
        self.draw_bar();
    }

    fn close_menu(&mut self) {
        if self.menu.take().is_some() {
            tracing::info!("menu fechado");
            self.draw_bar();
        }
    }

    /// Centro do título ou ícone da barra de onde o menu `id` vem.
    fn anchor_of(&self, id: MenuId) -> Option<f32> {
        if id == MenuId::System {
            return Some(LOGO_X + LOGO_W / 2.0);
        }
        self.titles.iter().find(|(t, _, _)| *t == id).map(|(_, x0, x1)| (x0 + x1) / 2.0)
    }

    /// Menu cujo título fica em `x` na barra (a marca do mide é o menu do sistema).
    fn title_at(&self, x: f64) -> Option<MenuId> {
        let x = x as f32;
        if x >= LOGO_X - 6.0 && x <= LOGO_X + LOGO_W + 6.0 {
            return Some(MenuId::System);
        }
        self.titles.iter().find(|(_, x0, x1)| x >= *x0 && x <= *x1).map(|(id, _, _)| *id)
    }

    fn run(&mut self, action: MenuAction) {
        self.close_menu();
        tracing::info!("menu: {action:?}");
        match action {
            MenuAction::About => spawn_sibling("mide-about"),
            MenuAction::Native(item) => {
                match (&self.appmenu, &self.native, &self.native_gtk) {
                    (Some((_, conn)), _, Some((menu, actions))) => {
                        if let Some(action) = usize::try_from(item).ok().and_then(|i| actions.get(i)) {
                            mide::gmenu::activate(conn, menu, action);
                        }
                    }
                    (Some((_, conn)), Some((sender, path, _)), None) => mide::appmenu::click(conn, sender, path, item),
                    _ => {}
                }
            }
            MenuAction::Tray(index, id) => {
                if let (Some(tray), Some(item)) = (&self.tray, self.tray_items.get(index as usize)) {
                    tray.click(item, id);
                }
            }
            MenuAction::NewWindow => match self.names.exec_for(&self.focused_id) {
                Some(command) => {
                    if let Ok(mut child) = std::process::Command::new("sh").arg("-c").arg(&command).spawn() {
                        std::thread::spawn(move || {
                            let _ = child.wait();
                        });
                    }
                }
                None => tracing::warn!("sem comando para abrir outra janela de {}", self.focused_id),
            },
            MenuAction::Compositor(command) => {
                let _ = ask_compositor(&command);
            }
            MenuAction::Settings(panel) => system::open_settings(panel),
            MenuAction::Power(action) => system::power(action),
            MenuAction::Confirm(id) => self.open_menu(id),
            MenuAction::ForceQuit => {
                let _ = ask_compositor("force_quit");
            }
            MenuAction::AppWindows(what) => {
                let mut ids = vec![self.focused_id.to_lowercase()];
                if let Some(last) = self.focused_id.rsplit('.').next() {
                    ids.push(last.to_lowercase());
                }
                let _ = ask_compositor(&format!("{what} {}", ids.join(" ")));
            }
        }
    }
}

impl Bar {
    fn request_status(&self) {
        self.status_now.store(true, std::sync::atomic::Ordering::Relaxed);
    }

    /// Desenha o painel da direita aberto (calendário, som, Wi-Fi ou Bluetooth).
    fn draw_custom_panel(&mut self) {
        let Some(menu) = self.menu.as_ref() else {
            return;
        };
        let (w, h, id, px, pw, hover) = (menu.width, menu.height, menu.id, menu.x, menu.panel_w, menu.hover_region);
        if w == 0 || h == 0 {
            return;
        }
        let Some(mut pixmap) = Pixmap::new(w, h) else {
            return;
        };
        let top = PANEL_TOP;
        // Conteúdo primeiro num pixmap à parte, para saber a altura do painel.
        let mut content = Pixmap::new(pw as u32, 900).unwrap_or_else(|| Pixmap::new(1, 1).unwrap());
        // Altura que o painel pode usar (para a central de notificações caber na tela).
        self.panel_room = h as f32 - PANEL_TOP - 12.0;
        let mut regions = Vec::new();
        let ph = match id {
            MenuId::Calendar => self.panel_calendar(&mut content, pw, &mut regions, hover),
            MenuId::Sound => self.panel_sound(&mut content, pw, &mut regions, hover),
            MenuId::Wifi => self.panel_wifi(&mut content, pw, &mut regions, hover),
            MenuId::Control => self.panel_control(&mut content, pw, &mut regions, hover),
            _ if id.confirm() => self.panel_confirm(&mut content, pw, id, &mut regions, hover),
            _ => self.panel_bluetooth(&mut content, pw, &mut regions, hover),
        };
        // Confirmações no meio da tela (na horizontal e na vertical); o resto, abaixo
        // da barra.
        let top = if id.confirm() { ((h as f32 - ph) / 2.0).round().max(PANEL_TOP) } else { top };
        if let Some(menu) = self.menu.as_mut() {
            menu.panel_top = top;
        }
        // Sombra, vidro claro e borda fina, como os menus.
        for i in 0..6 {
            let spread = 6.0 - i as f32;
            fill_rounded(&mut pixmap, px - spread, top - spread + 3.0, pw + 2.0 * spread, ph + 2.0 * spread, 12.0 + spread, rgba(0, 0, 0, 6));
        }
        let glass = mide::control_client::panel_glass();
        let panel = [(px, top, pw, ph, 12.0)];
        self.glass_menu.set("mide-bar-menu", if glass { &panel } else { &[] });
        if glass {
            mide::ui::draw::clear_rounded(&mut pixmap, px, top, pw, ph, 12.0);
        }
        let fill = pal().panel(if glass { 186 } else { 242 });
        fill_rounded(&mut pixmap, px, top, pw, ph, 12.0, rgba(fill[0], fill[1], fill[2], fill[3]));
        if let Some(path) = rounded_rect(px + 0.5, top + 0.5, pw - 1.0, ph - 1.0, 12.0) {
            let mut border = Paint::default();
            border.set_color(pal().ink(40));
            border.anti_alias = true;
            pixmap.stroke_path(&path, &border, &Stroke::default(), Transform::identity(), None);
        }
        // Ponta para o ícone de onde o painel veio (confirmações ficam no meio, sem).
        if !id.confirm() {
            if let Some(cx) = self.anchor_of(id) {
                mide::ui::draw::menu_arrow(&mut pixmap, cx.clamp(px + 20.0, px + pw - 20.0), top, false, fill);
            }
        }
        pixmap.draw_pixmap(px as i32, top as i32, content.as_ref(), &Default::default(), Transform::identity(), None);
        if let Some(menu) = self.menu.as_mut() {
            menu.panel_h = ph;
            menu.regions = regions
                .into_iter()
                .map(|((rx, ry, rw, rh), a)| ((rx + px, ry + top, rw, rh), a))
                .collect();
        }
        self.present_layer(&pixmap, false);
    }

    /// Linha clicável com destaque azul sob o ponteiro (coordenadas do painel).
    #[allow(clippy::too_many_arguments)]
    fn panel_row(&mut self, p: &mut Pixmap, pw: f32, y: f32, h: f32, label: &str, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>, action: PanelAction) -> bool {
        let hovered = hover == Some(regions.len());
        if hovered {
            fill_rounded(p, 6.0, y, pw - 12.0, h, 6.0, pal().accent_color());
        }
        let color = if hovered { [255, 255, 255, 255] } else { pal().text };
        self.text.draw(p, label, 16.0, y + (h - FONT * 1.25) / 2.0, FONT, FontWeight::Regular, color);
        regions.push(((6.0, y, pw - 12.0, h), action));
        hovered
    }

    fn panel_separator(p: &mut Pixmap, pw: f32, y: f32) {
        fill_rounded(p, 12.0, y + 4.0, pw - 24.0, 1.0, 0.0, pal().ink(30));
    }

    /// Título do painel com um interruptor à direita.
    fn panel_header(&mut self, p: &mut Pixmap, pw: f32, title: &str, on: Option<bool>, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, action: PanelAction) {
        self.text.draw(p, title, 16.0, 14.0, 14.0, FontWeight::Bold, pal().text);
        if let Some(on) = on {
            draw_switch(p, pw - 54.0, 13.0, on);
            regions.push(((pw - 60.0, 8.0, 50.0, 30.0), action));
        }
    }

    fn panel_calendar(&mut self, p: &mut Pixmap, pw: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        if self.clock_options {
            return self.panel_clock_options(p, pw, regions, hover);
        }
        let today = Local::now().date_naive();
        let (year, month) = self.cal_month;
        self.text.draw(p, &calendar::long_date(today), 16.0, 12.0, 13.0, FontWeight::Regular, pal().secondary);
        let title = calendar::month_title(year, month);
        self.text.draw(p, &title, 16.0, 36.0, 15.0, FontWeight::Bold, pal().text);
        // Botões: ‹  Hoje  ›.
        let buttons = [("‹", pw - 98.0, 26.0, PanelAction::PrevMonth), ("Hoje", pw - 70.0, 40.0, PanelAction::Today), ("›", pw - 28.0, 22.0, PanelAction::NextMonth)];
        for (label, bx, bw, action) in buttons {
            if hover == Some(regions.len()) {
                fill_rounded(p, bx, 34.0, bw, 24.0, 6.0, rgba(0, 0, 0, 22));
            }
            let size = if label.len() > 2 { 12.5 } else { 17.0 };
            let lw = self.text.measure(label, size, FontWeight::Semibold);
            self.text.draw(p, label, bx + (bw - lw) / 2.0, 46.0 - size * 0.62, size, FontWeight::Semibold, pal().accent);
            regions.push(((bx, 34.0, bw, 24.0), action));
        }
        let cell = (pw - 24.0) / 7.0;
        for (i, wd) in calendar::WEEKDAYS.iter().enumerate() {
            let ww = self.text.measure(wd, 11.5, FontWeight::Semibold);
            self.text.draw(p, wd, 12.0 + i as f32 * cell + (cell - ww) / 2.0, 68.0, 11.5, FontWeight::Semibold, pal().secondary);
        }
        for (i, day) in calendar::month_grid(year, month).iter().enumerate() {
            let (col, row) = ((i % 7) as f32, (i / 7) as f32);
            let cx = 12.0 + col * cell + cell / 2.0;
            let cy = 98.0 + row * 30.0;
            let label = day.date.day().to_string();
            let lw = self.text.measure(&label, 13.0, FontWeight::Regular);
            let mut color = if day.in_month { pal().text } else { pal().faint };
            if day.date == today {
                if let Some(c) = PathBuilder::from_circle(cx, cy, 13.0) {
                    let mut paint = Paint::default();
                    paint.set_color(pal().accent_color());
                    paint.anti_alias = true;
                    p.fill_path(&c, &paint, FillRule::Winding, Transform::identity(), None);
                }
                color = [255, 255, 255, 255];
            }
            self.text.draw(p, &label, cx - lw / 2.0, cy - 8.5, 13.0, if day.date == today { FontWeight::Bold } else { FontWeight::Regular }, color);
        }
        let y = 98.0 + 5.0 * 30.0 + 14.0;
        Self::panel_separator(p, pw, y);
        self.panel_row(p, pw, y + 10.0, 26.0, "Formato da hora…", regions, hover, PanelAction::ClockOptions);
        let y = y + 10.0 + 26.0 + 8.0;
        self.panel_notifications(p, pw, y, regions, hover)
    }

    /// Central de notificações: as mais recentes (também as silenciadas pelo Não
    /// perturbe), com "Limpar".
    fn panel_notifications(&mut self, p: &mut Pixmap, pw: f32, y: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        Self::panel_separator(p, pw, y);
        let mut y = y + 12.0;
        self.text.draw(p, "Notificações", 16.0, y, 13.5, FontWeight::Bold, pal().text);
        if !self.history.is_empty() {
            let lw = self.text.measure("Limpar", 12.5, FontWeight::Semibold);
            let bx = pw - 16.0 - lw;
            if hover == Some(regions.len()) {
                fill_rounded(p, bx - 6.0, y - 2.0, lw + 12.0, 22.0, 6.0, rgba(0, 0, 0, 22));
            }
            self.text.draw(p, "Limpar", bx, y + 1.0, 12.5, FontWeight::Semibold, pal().accent);
            regions.push(((bx - 6.0, y - 2.0, lw + 12.0, 22.0), PanelAction::ClearNotifications));
        }
        y += 26.0;
        if self.history.is_empty() {
            self.text.draw(p, "Nenhuma notificação", 16.0, y, 12.5, FontWeight::Regular, pal().secondary);
            return y + 26.0;
        }
        let now = std::time::SystemTime::now();
        // Só as que cabem na tela (até 5).
        let fits = (((self.panel_room - y - 30.0) / 62.0).floor().max(1.0) as usize).min(5);
        for (i, entry) in self.history.clone().iter().take(fits).enumerate() {
            // Cartão claro com app e hora, título e a primeira linha do texto. Clicável
            // quando há o que abrir (o arquivo salvo ou o app que avisou).
            let clickable = !entry.open.is_empty() || !entry.app_id.is_empty();
            let hovered = clickable && hover == Some(regions.len());
            fill_rounded(p, 10.0, y, pw - 20.0, 56.0, 10.0, rgba(255, 255, 255, if hovered { 200 } else { 140 }));
            if clickable {
                regions.push(((10.0, y, pw - 20.0, 56.0), PanelAction::Notification(i)));
            }
            let when = mide::files::date_text(std::time::UNIX_EPOCH + Duration::from_secs(entry.time), now);
            let ww = self.text.measure(&when, 11.0, FontWeight::Regular);
            let app = fit_label(&mut self.text, &entry.app, 11.0, pw - 52.0 - ww);
            self.text.draw(p, &app, 20.0, y + 6.0, 11.0, FontWeight::Regular, pal().secondary);
            self.text.draw(p, &when, pw - 20.0 - ww, y + 6.0, 11.0, FontWeight::Regular, pal().secondary);
            let summary = fit_label(&mut self.text, &entry.summary, 13.0, pw - 40.0);
            self.text.draw(p, &summary, 20.0, y + 21.0, 13.0, FontWeight::Semibold, pal().text);
            let first = entry.body.lines().next().unwrap_or("");
            let body = fit_label(&mut self.text, first, 12.0, pw - 40.0);
            self.text.draw(p, &body, 20.0, y + 37.0, 12.0, FontWeight::Regular, pal().body);
            y += 62.0;
        }
        if self.history.len() > fits {
            let more = format!("e mais {}", self.history.len() - fits);
            self.text.draw(p, &more, 16.0, y, 11.5, FontWeight::Regular, pal().secondary);
            y += 20.0;
        }
        y + 6.0
    }

    /// Opções do relógio da barra: o que aparece além da hora.
    fn panel_clock_options(&mut self, p: &mut Pixmap, pw: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        self.text.draw(p, "Formato da hora", 16.0, 14.0, 14.0, FontWeight::Bold, pal().text);
        let preview = clock_text();
        self.text.draw(p, &preview, 16.0, 38.0, 12.5, FontWeight::Regular, pal().secondary);
        let mut y = 64.0;
        Self::panel_separator(p, pw, y - 8.0);
        for (i, (key, label)) in CLOCK_PARTS.iter().enumerate() {
            self.text.draw(p, label, 16.0, y + 6.0, FONT, FontWeight::Regular, pal().text);
            draw_switch(p, pw - 54.0, y + 4.0, clock_part(key));
            regions.push(((pw - 60.0, y, 50.0, 30.0), PanelAction::ClockPart(i)));
            y += 32.0;
        }
        Self::panel_separator(p, pw, y);
        self.panel_row(p, pw, y + 10.0, 26.0, "‹ Calendário", regions, hover, PanelAction::ClockOptions);
        y + 10.0 + 26.0 + 8.0
    }

    fn panel_sound(&mut self, p: &mut Pixmap, pw: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        let (volume, muted) = self.status.volume.unwrap_or((0.0, false));
        self.panel_header(p, pw, "Som", None, regions, PanelAction::ToggleMute);
        // Trilho do volume com o alto-falante à esquerda.
        draw_speaker_icon(p, 24.0, 58.0, Some((volume, muted)), pal().body);
        let (tx, tw) = (44.0, pw - 64.0);
        fill_rounded(p, tx, 54.0, tw, 8.0, 4.0, pal().ink(30));
        let level = if muted { 0.0 } else { volume.clamp(0.0, 1.0) };
        fill_rounded(p, tx, 54.0, tw * level, 8.0, 4.0, pal().accent_color());
        let kx = tx + tw * level;
        if let Some(knob) = PathBuilder::from_circle(kx, 58.0, 9.0) {
            let mut paint = Paint::default();
            paint.anti_alias = true;
            paint.set_color(rgba(255, 255, 255, 255));
            p.fill_path(&knob, &paint, FillRule::Winding, Transform::identity(), None);
            paint.set_color(rgba(0, 0, 0, 60));
            p.stroke_path(&knob, &paint, &Stroke { width: 1.0, ..Default::default() }, Transform::identity(), None);
        }
        regions.push(((tx - 8.0, 44.0, tw + 16.0, 28.0), PanelAction::VolumeTrack));
        // "Sem som" com interruptor.
        self.text.draw(p, "Sem som", 16.0, 86.0, FONT, FontWeight::Regular, pal().text);
        draw_switch(p, pw - 54.0, 84.0, muted);
        regions.push(((pw - 60.0, 80.0, 50.0, 28.0), PanelAction::ToggleMute));
        Self::panel_separator(p, pw, 114.0);
        // Saída (como no Mac): cada aparelho num círculo, o escolhido em azul.
        let mut y = 124.0;
        let outputs = self.status.outputs.clone();
        if !outputs.is_empty() {
            self.text.draw(p, "Saída", 16.0, y, 12.0, FontWeight::Semibold, pal().secondary);
            y += 22.0;
            for (i, device) in outputs.iter().enumerate() {
                y = self.audio_row(p, pw, y, device, regions, hover, PanelAction::AudioOutput(i));
            }
            Self::panel_separator(p, pw, y);
            y += 10.0;
        }
        // Entrada: os microfones, do mesmo jeito.
        let inputs = self.status.inputs.clone();
        if inputs.len() > 1 {
            self.text.draw(p, "Entrada", 16.0, y, 12.0, FontWeight::Semibold, pal().secondary);
            y += 22.0;
            for (i, device) in inputs.iter().enumerate() {
                y = self.audio_row(p, pw, y, device, regions, hover, PanelAction::AudioInput(i));
            }
            Self::panel_separator(p, pw, y);
            y += 10.0;
        }
        self.panel_row(p, pw, y, 26.0, "Configurações de som…", regions, hover, PanelAction::Settings("sound"));
        y + 26.0 + 8.0
    }

    /// Uma saída ou entrada no painel de som: ícone do tipo num círculo (azul na
    /// escolhida), nome e, se houver, a linha de baixo (conector, placa).
    #[allow(clippy::too_many_arguments)]
    fn audio_row(&mut self, p: &mut Pixmap, pw: f32, y: f32, device: &system::AudioDevice, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>, action: PanelAction) -> f32 {
        let hovered = hover == Some(regions.len());
        if hovered {
            fill_rounded(p, 6.0, y, pw - 12.0, 34.0, 8.0, pal().ink(18));
        }
        let circle = if device.default { pal().accent_color() } else { pal().ink(26) };
        fill_rounded(p, 14.0, y + 4.0, 26.0, 26.0, 13.0, circle);
        let fg = if device.default { [255, 255, 255, 255] } else { pal().text };
        draw_audio_kind_icon(p, 27.0, y + 17.0, device.kind, fg);
        let name = fit_label(&mut self.text, &device.name, FONT, pw - 64.0);
        if device.detail.is_empty() {
            self.text.draw(p, &name, 50.0, y + (34.0 - FONT * 1.25) / 2.0, FONT, FontWeight::Regular, pal().text);
        } else {
            self.text.draw(p, &name, 50.0, y + 1.0, FONT, FontWeight::Regular, pal().text);
            let detail = fit_label(&mut self.text, &device.detail, 11.0, pw - 64.0);
            self.text.draw(p, &detail, 50.0, y + 18.0, 11.0, FontWeight::Regular, pal().secondary);
        }
        regions.push(((6.0, y, pw - 12.0, 34.0), action));
        y + 36.0
    }

    fn panel_wifi(&mut self, p: &mut Pixmap, pw: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        let (on, networks) = self.status.wifi.clone().unwrap_or((false, Vec::new()));
        self.panel_header(p, pw, "Wi-Fi", Some(on), regions, PanelAction::ToggleWifi);
        let mut y = 46.0;
        Self::panel_separator(p, pw, y - 6.0);
        if on {
            if networks.is_empty() {
                self.text.draw(p, "Procurando redes…", 16.0, y + 4.0, FONT, FontWeight::Regular, pal().secondary);
                y += 28.0;
            }
            for net in networks.iter().take(7) {
                let hovered = self.panel_row(p, pw, y, 30.0, "", regions, hover, PanelAction::Connect(net.ssid.clone()));
                let color = if hovered { [255, 255, 255, 255] } else { pal().text };
                let level = 1 + (net.signal as u32 * 3 / 100).min(2) as u8;
                draw_wifi_icon(p, 26.0, y + 15.0, Some(level), color);
                let weight = if net.active { FontWeight::Semibold } else { FontWeight::Regular };
                self.text.draw(p, &net.ssid, 44.0, y + (30.0 - FONT * 1.25) / 2.0, FONT, weight, color);
                if net.secure {
                    draw_lock(p, pw - 40.0, y + 15.0, color);
                }
                if net.active {
                    self.text.draw(p, "✓", pw - 24.0, y + 6.0, FONT, FontWeight::Bold, color);
                }
                y += 30.0;
            }
        } else {
            self.text.draw(p, "Wi-Fi desligado", 16.0, y + 4.0, FONT, FontWeight::Regular, pal().secondary);
            y += 28.0;
        }
        // Rede cabeada (como o Mac mostra a Ethernet): cada placa com cabo, o estado e
        // o endereço.
        let wired: Vec<system::Wired> = self.status.wired.iter().filter(|w| w.state != system::WiredState::Unplugged).cloned().collect();
        if !wired.is_empty() {
            Self::panel_separator(p, pw, y);
            y += 10.0;
            self.text.draw(p, "Rede cabeada", 16.0, y, 12.0, FontWeight::Semibold, pal().secondary);
            y += 22.0;
            for w in &wired {
                let connected = w.state == system::WiredState::Connected;
                let circle = if connected { pal().accent_color() } else { pal().ink(26) };
                fill_rounded(p, 14.0, y + 4.0, 26.0, 26.0, 13.0, circle);
                draw_ethernet_icon(p, 27.0, y + 17.0, if connected { [255, 255, 255, 255] } else { pal().text });
                self.text.draw(p, "Ethernet", 50.0, y + 1.0, FONT, FontWeight::Regular, pal().text);
                let detail = match w.state {
                    system::WiredState::Connected => match &w.address {
                        Some(ip) => format!("Conectado · {ip}"),
                        None => "Conectado".to_string(),
                    },
                    system::WiredState::Connecting => "Conectando…".to_string(),
                    _ => "Cabo ligado, sem conexão".to_string(),
                };
                let detail = fit_label(&mut self.text, &detail, 11.0, pw - 64.0);
                self.text.draw(p, &detail, 50.0, y + 18.0, 11.0, FontWeight::Regular, pal().secondary);
                y += 36.0;
            }
        }
        Self::panel_separator(p, pw, y);
        y += 10.0;
        self.panel_row(p, pw, y, 26.0, "Configurações de rede…", regions, hover, PanelAction::Settings("network"));
        y + 26.0 + 8.0
    }

    fn panel_bluetooth(&mut self, p: &mut Pixmap, pw: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        let (on, devices) = self.status.bluetooth.clone().unwrap_or((false, Vec::new()));
        self.panel_header(p, pw, "Bluetooth", Some(on), regions, PanelAction::ToggleBluetooth);
        let mut y = 46.0;
        Self::panel_separator(p, pw, y - 6.0);
        if !on {
            self.text.draw(p, "Bluetooth desligado", 16.0, y + 4.0, FONT, FontWeight::Regular, pal().secondary);
            y += 28.0;
        } else if devices.is_empty() {
            self.text.draw(p, "Nenhum aparelho pareado", 16.0, y + 4.0, FONT, FontWeight::Regular, pal().secondary);
            y += 28.0;
        }
        if on {
            for (i, device) in devices.iter().enumerate().take(7) {
                let hovered = self.panel_row(p, pw, y, 30.0, "", regions, hover, PanelAction::Device(i));
                let color = if hovered { [255, 255, 255, 255] } else { pal().text };
                // Círculo azul para aparelho conectado, cinza para os outros.
                let dot = if device.connected { pal().accent_color() } else { rgba(170, 170, 176, 255) };
                fill_rounded(p, 16.0, y + 7.0, 16.0, 16.0, 8.0, if hovered { rgba(255, 255, 255, 255) } else { dot });
                self.text.draw(p, &device.name, 42.0, y + (30.0 - FONT * 1.25) / 2.0, FONT, FontWeight::Regular, color);
                if device.connected {
                    let lw = self.text.measure("Conectado", 11.5, FontWeight::Regular);
                    let dc = if hovered { [230, 236, 255, 255] } else { pal().secondary };
                    self.text.draw(p, "Conectado", pw - 16.0 - lw, y + 8.0, 11.5, FontWeight::Regular, dc);
                }
                y += 30.0;
            }
        }
        Self::panel_separator(p, pw, y);
        y += 10.0;
        self.panel_row(p, pw, y, 26.0, "Configurações de Bluetooth…", regions, hover, PanelAction::Settings("bluetooth"));
        y + 26.0 + 8.0
    }

    /// Central de controle, como a do macOS: ladrilhos de Wi-Fi e Bluetooth, Não
    /// perturbe, brilho da tela (se houver controle), som e as Configurações.
    fn panel_control(&mut self, p: &mut Pixmap, pw: f32, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        // Tahoe: módulos bem arredondados.
        let tile = |p: &mut Pixmap, x: f32, y: f32, w: f32, h: f32| fill_rounded(p, x, y, w, h, 18.0, pal().tile());
        let dark = pal().text;
        let gray = pal().secondary;
        // Botão redondo de ligar/desligar: azul quando ligado.
        let round = |p: &mut Pixmap, cx: f32, cy: f32, on: bool| {
            if let Some(c) = PathBuilder::from_circle(cx, cy, 15.0) {
                let mut paint = Paint::default();
                paint.anti_alias = true;
                paint.set_color(if on { pal().accent_color() } else { pal().ink(28) });
                p.fill_path(&c, &paint, FillRule::Winding, Transform::identity(), None);
            }
        };
        let (half, gap) = ((pw - 36.0) / 2.0, 12.0);
        // Ladrilho da esquerda: Wi-Fi e Bluetooth.
        tile(p, gap, gap, half, 104.0);
        let (wifi_on, _) = self.status.wifi.clone().unwrap_or((false, Vec::new()));
        let net = self.status.active_network().map(|n| n.ssid.clone());
        round(p, gap + 26.0, gap + 28.0, wifi_on);
        draw_wifi_icon(p, gap + 26.0, gap + 28.0, wifi_on.then_some(3), if wifi_on { [255, 255, 255, 255] } else { dark });
        self.text.draw(p, "Wi-Fi", gap + 48.0, gap + 13.0, FONT, FontWeight::Semibold, dark);
        let wifi_detail = if !wifi_on { "Desligado".to_string() } else { net.unwrap_or_else(|| "Sem rede".into()) };
        let wifi_detail = fit_label(&mut self.text, &wifi_detail, 11.5, half - 56.0);
        self.text.draw(p, &wifi_detail, gap + 48.0, gap + 31.0, 11.5, FontWeight::Regular, gray);
        regions.push(((gap, gap, half, 52.0), PanelAction::ToggleWifi));
        let bt_on = self.status.bluetooth.as_ref().is_some_and(|(on, _)| *on);
        round(p, gap + 26.0, gap + 76.0, bt_on);
        draw_bluetooth_icon(p, gap + 26.0, gap + 76.0, true, if bt_on { [255, 255, 255, 255] } else { dark });
        self.text.draw(p, "Bluetooth", gap + 48.0, gap + 61.0, FONT, FontWeight::Semibold, dark);
        self.text.draw(p, if bt_on { "Ligado" } else { "Desligado" }, gap + 48.0, gap + 79.0, 11.5, FontWeight::Regular, gray);
        regions.push(((gap, gap + 52.0, half, 52.0), PanelAction::ToggleBluetooth));
        // Ladrilho da direita: Não perturbe.
        let dnd = dnd_on();
        let rx = gap * 2.0 + half;
        tile(p, rx, gap, half, 104.0);
        round(p, rx + 26.0, gap + 52.0, dnd);
        draw_moon_icon(p, rx + 26.0, gap + 52.0, if dnd { [255, 255, 255, 255] } else { dark });
        self.text.draw(p, "Não", rx + 48.0, gap + 37.0, FONT, FontWeight::Semibold, dark);
        self.text.draw(p, "perturbe", rx + 48.0, gap + 54.0, FONT, FontWeight::Semibold, dark);
        regions.push(((rx, gap, half, 104.0), PanelAction::ToggleDnd));
        let mut y = gap + 104.0 + gap;
        // Trilhos: brilho (se houver controle) e som.
        // Tahoe: o deslizante é uma cápsula larga; a parte cheia é na cor de destaque do
        // mide (pedido do usuário: o azul padrão, não branco), com um botão branco na
        // ponta que anda com o valor.
        let mut slider = |this: &mut Self, p: &mut Pixmap, y: f32, title: &str, value: f32, action: PanelAction| {
            tile(p, gap, y, pw - 2.0 * gap, 60.0);
            this.text.draw(p, title, gap + 14.0, y + 8.0, FONT, FontWeight::Semibold, dark);
            let (tx, tw, th) = (gap + 12.0, pw - 2.0 * gap - 24.0, 22.0);
            let ty = y + 30.0;
            fill_rounded(p, tx, ty, tw, th, th / 2.0, pal().ink(26));
            let filled = (tw * value).max(th);
            fill_rounded(p, tx, ty, filled, th, th / 2.0, pal().accent_color());
            let knob = th - 4.0;
            fill_rounded(p, tx + filled - knob - 2.0, ty + 2.0, knob, knob, knob / 2.0, rgba(255, 255, 255, 255));
            if let Some(edge) = rounded_rect(tx + 0.5, ty + 0.5, tw - 1.0, th - 1.0, th / 2.0 - 0.5) {
                let mut paint = Paint::default();
                paint.anti_alias = true;
                paint.set_color(pal().ink(22));
                p.stroke_path(&edge, &paint, &Stroke::default(), Transform::identity(), None);
            }
            // Com 8 de folga dos lados (`track_from_x` conta com ela).
            regions.push(((tx - 8.0, ty - 3.0, tw + 16.0, th + 6.0), action));
        };
        if let Some(level) = self.status.brightness {
            slider(self, p, y, "Tela", level, PanelAction::BrightnessTrack);
            y += 60.0 + gap;
        }
        if let Some((volume, muted)) = self.status.volume {
            slider(self, p, y, "Som", if muted { 0.0 } else { volume.clamp(0.0, 1.0) }, PanelAction::VolumeTrack);
            y += 60.0 + gap;
        }
        Self::panel_separator(p, pw, y - 6.0);
        self.panel_row(p, pw, y + 4.0, 26.0, "Configurações do Sistema…", regions, hover, PanelAction::Settings(""));
        y + 4.0 + 26.0 + 8.0
    }

    /// Confirmação antes de reiniciar, desligar ou encerrar a sessão.
    fn panel_confirm(&mut self, p: &mut Pixmap, pw: f32, id: MenuId, regions: &mut Vec<((f32, f32, f32, f32), PanelAction)>, hover: Option<usize>) -> f32 {
        let (title, button) = match id {
            MenuId::ConfirmRestart => ("Reiniciar o computador agora?", "Reiniciar"),
            MenuId::ConfirmShutdown => ("Desligar o computador agora?", "Desligar"),
            _ => ("Encerrar a sessão agora?", "Encerrar sessão"),
        };
        self.text.draw(p, title, 18.0, 18.0, 14.5, FontWeight::Bold, pal().text);
        self.text.draw(p, "Os apps abertos serão fechados.", 18.0, 42.0, 12.5, FontWeight::Regular, pal().secondary);
        let (bw, by) = ((pw - 48.0) / 2.0, 74.0);
        let cancel_hover = hover == Some(regions.len());
        fill_rounded(p, 18.0, by, bw, 30.0, 8.0, pal().ink(if cancel_hover { 36 } else { 20 }));
        let cw = self.text.measure("Cancelar", FONT, FontWeight::Regular);
        self.text.draw(p, "Cancelar", 18.0 + (bw - cw) / 2.0, by + 6.0, FONT, FontWeight::Regular, pal().text);
        regions.push(((18.0, by, bw, 30.0), PanelAction::ConfirmNo));
        let ok_hover = hover == Some(regions.len());
        let ox = 30.0 + bw;
        fill_rounded(p, ox, by, bw, 30.0, 8.0, if ok_hover { pal().accent_pressed() } else { pal().accent_color() });
        let ow = self.text.measure(button, FONT, FontWeight::Semibold);
        self.text.draw(p, button, ox + (bw - ow) / 2.0, by + 6.0, FONT, FontWeight::Semibold, [255, 255, 255, 255]);
        regions.push(((ox, by, bw, 30.0), PanelAction::ConfirmYes));
        by + 30.0 + 18.0
    }

    fn region_at(&self, x: f64, y: f64) -> Option<usize> {
        let menu = self.menu.as_ref()?;
        let (x, y) = (x as f32, y as f32);
        menu.regions.iter().position(|((rx, ry, rw, rh), _)| x >= *rx && x <= rx + rw && y >= *ry && y <= ry + rh)
    }

    /// Volume pela posição x no trilho do painel de som.
    fn volume_from_x(&self, x: f64) -> Option<f32> {
        self.track_from_x(x, PanelAction::VolumeTrack)
    }

    /// Valor (0 a 1) pela posição x no trilho `action` do painel aberto.
    fn track_from_x(&self, x: f64, action: PanelAction) -> Option<f32> {
        let menu = self.menu.as_ref()?;
        let ((rx, _, rw, _), _) = menu.regions.iter().find(|(_, a)| *a == action)?;
        let (tx, tw) = (rx + 8.0, rw - 16.0);
        Some(((x as f32 - tx) / tw).clamp(0.0, 1.0))
    }

    fn custom_motion(&mut self, x: f64, y: f64) {
        // Na faixa da barra, passar por outro título troca de painel.
        if y < BAR_HEIGHT as f64 {
            if let Some(id) = self.title_at(x) {
                if self.menu.as_ref().is_some_and(|m| m.id != id) {
                    self.open_menu(id);
                    return;
                }
            }
        }
        if self.menu.as_ref().is_some_and(|m| m.dragging_brightness) {
            if let Some(value) = self.track_from_x(x, PanelAction::BrightnessTrack) {
                self.status.brightness = Some(value);
                if self.volume_sent.elapsed() >= Duration::from_millis(80) {
                    system::set_brightness(value);
                    self.volume_sent = std::time::Instant::now();
                }
                self.draw_menu();
            }
            return;
        }
        if self.menu.as_ref().is_some_and(|m| m.dragging_volume) {
            if let Some(value) = self.volume_from_x(x) {
                self.status.volume = Some((value, false));
                if self.volume_sent.elapsed() >= Duration::from_millis(80) {
                    system::set_volume(value);
                    self.volume_sent = std::time::Instant::now();
                }
                self.draw_menu();
                self.draw_bar();
            }
            return;
        }
        let hover = self.region_at(x, y);
        if self.menu.as_ref().is_some_and(|m| m.hover_region != hover) {
            if let Some(menu) = self.menu.as_mut() {
                menu.hover_region = hover;
            }
            self.draw_menu();
        }
    }

    fn custom_press(&mut self, x: f64, y: f64) {
        let inside = self.menu.as_ref().is_some_and(|m| m.inside_panel(x, y));
        if y < BAR_HEIGHT as f64 {
            match self.title_at(x) {
                Some(id) if self.menu.as_ref().is_some_and(|m| m.id == id) => self.close_menu(),
                Some(id) => self.open_menu(id),
                None => self.close_menu(),
            }
            return;
        }
        if !inside {
            self.close_menu();
            return;
        }
        let Some(action) = self.region_at(x, y).and_then(|i| self.menu.as_ref().map(|m| m.regions[i].1.clone())) else {
            return;
        };
        tracing::info!("painel: {action:?}");
        match action {
            PanelAction::PrevMonth => self.cal_month = calendar::shift_month(self.cal_month.0, self.cal_month.1, -1),
            PanelAction::NextMonth => self.cal_month = calendar::shift_month(self.cal_month.0, self.cal_month.1, 1),
            PanelAction::Today => {
                let today = Local::now().date_naive();
                self.cal_month = (today.year(), today.month());
            }
            PanelAction::VolumeTrack => {
                if let Some(value) = self.volume_from_x(x) {
                    // Mexer no volume tira o mudo, como no macOS.
                    let was_muted = self.status.volume.is_some_and(|(_, m)| m);
                    self.status.volume = Some((value, false));
                    system::set_volume(value);
                    if was_muted {
                        system::set_muted(false);
                    }
                    self.volume_sent = std::time::Instant::now();
                    if let Some(menu) = self.menu.as_mut() {
                        menu.dragging_volume = true;
                    }
                }
            }
            PanelAction::Notification(i) => {
                if let Some(entry) = self.history.get(i).cloned() {
                    tracing::info!("central de notificações: aviso {i} clicado");
                    self.close_menu();
                    std::thread::spawn(move || mide::notify::activate(&entry.open, &entry.app_id));
                    return;
                }
            }
            PanelAction::AudioOutput(i) => {
                if let Some(device) = self.status.outputs.get(i).cloned() {
                    tracing::info!("som: saída {}", device.name);
                    for (j, d) in self.status.outputs.iter_mut().enumerate() {
                        d.default = j == i;
                    }
                    // O volume mostrado passa a ser o da saída escolhida.
                    if let Some((_, m)) = self.status.volume {
                        self.status.volume = Some((device.volume, m));
                    }
                    system::select_audio(&device, true);
                }
            }
            PanelAction::AudioInput(i) => {
                if let Some(device) = self.status.inputs.get(i).cloned() {
                    tracing::info!("som: entrada {}", device.name);
                    for (j, d) in self.status.inputs.iter_mut().enumerate() {
                        d.default = j == i;
                    }
                    system::select_audio(&device, false);
                }
            }
            PanelAction::ToggleMute => {
                if let Some((v, m)) = self.status.volume {
                    self.status.volume = Some((v, !m));
                    system::set_muted(!m);
                }
            }
            PanelAction::ToggleWifi => {
                if let Some((on, list)) = self.status.wifi.clone() {
                    self.status.wifi = Some((!on, list));
                    system::set_wifi(!on);
                }
            }
            PanelAction::Connect(ssid) => {
                system::connect_network(&ssid);
                self.close_menu();
            }
            PanelAction::ToggleBluetooth => {
                if let Some((on, list)) = self.status.bluetooth.clone() {
                    self.status.bluetooth = Some((!on, list));
                    system::set_bluetooth(!on);
                }
            }
            PanelAction::Device(i) => {
                if let Some(device) = self.status.bluetooth.as_ref().and_then(|(_, d)| d.get(i)).cloned() {
                    system::toggle_device(&device);
                }
                self.close_menu();
            }
            PanelAction::Settings(panel) => {
                system::open_settings(panel);
                self.close_menu();
            }
            PanelAction::ToggleDnd => {
                let on = !dnd_on();
                match mide::settings_file::set("notifications", "dnd", on.then_some(mide::settings_file::Value::Bool(true))) {
                    Ok(()) => tracing::info!("não perturbe: {}", if on { "ligado" } else { "desligado" }),
                    Err(err) => tracing::warn!("não perturbe não gravado: {err}"),
                }
            }
            PanelAction::BrightnessTrack => {
                if let Some(value) = self.track_from_x(x, PanelAction::BrightnessTrack) {
                    self.status.brightness = Some(value);
                    system::set_brightness(value);
                    self.volume_sent = std::time::Instant::now();
                    if let Some(menu) = self.menu.as_mut() {
                        menu.dragging_brightness = true;
                    }
                }
            }
            PanelAction::ClockOptions => self.clock_options = !self.clock_options,
            PanelAction::ClearNotifications => {
                mide::notify::clear_history();
                self.history.clear();
                tracing::info!("notificações limpas");
            }
            PanelAction::ClockPart(i) => {
                let (key, label) = CLOCK_PARTS[i];
                let on = !clock_part(key);
                match mide::settings_file::set("clock", key, on.then_some(mide::settings_file::Value::Bool(true))) {
                    Ok(()) => tracing::info!("relógio: {label} {}", if on { "ligado" } else { "desligado" }),
                    Err(err) => tracing::warn!("formato da hora não gravado: {err}"),
                }
                self.clock = clock_text();
            }
            PanelAction::ConfirmNo => self.close_menu(),
            PanelAction::ConfirmYes => {
                let id = self.menu.as_ref().map(|m| m.id);
                self.close_menu();
                match id {
                    Some(MenuId::ConfirmRestart) => system::power(system::Power::Restart),
                    Some(MenuId::ConfirmShutdown) => system::power(system::Power::Shutdown),
                    Some(MenuId::ConfirmLogout) => {
                        tracing::info!("encerrando a sessão");
                        let _ = ask_compositor("quit");
                    }
                    _ => {}
                }
            }
        }
        self.request_status();
        if self.menu.is_some() {
            self.draw_menu();
        }
        self.draw_bar();
    }
}

/// Não perturbe ligado (`[notifications] dnd`): as notificações não aparecem.
fn dnd_on() -> bool {
    matches!(mide::settings_file::get("notifications", "dnd"), Some(mide::settings_file::Value::Bool(true)))
}

/// Corta o texto com reticências para caber em `max`.
fn fit_label(text: &mut Text, value: &str, size: f32, max: f32) -> String {
    if text.measure(value, size, FontWeight::Regular) <= max {
        return value.to_string();
    }
    let mut chars: Vec<char> = value.chars().collect();
    while !chars.is_empty() {
        chars.pop();
        let candidate = format!("{}…", chars.iter().collect::<String>().trim_end());
        if text.measure(&candidate, size, FontWeight::Regular) <= max {
            return candidate;
        }
    }
    "…".into()
}

/// Ícone da central de controle: dois interruptores. Em cima, pílula vazada com a
/// bolinha cheia na ponta esquerda (desligado); embaixo, pílula cheia com a bolinha
/// vazada na ponta direita (ligado), como no macOS.
fn draw_control_icon(p: &mut Pixmap, cx: f32, cy: f32, fg: [u8; 4]) {
    let (w, h) = (17.0, 7.0);
    let (left, r) = (cx - w / 2.0, h / 2.0);
    let mut fill = Paint::default();
    fill.anti_alias = true;
    fill.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
    // Em cima: contorno e bolinha encostada na ponta esquerda.
    let top = cy - 4.3;
    if let Some(pill) = rounded_rect(left, top - r, w, h, r) {
        p.stroke_path(&pill, &stroke_color(fg, 255), &Stroke { width: 1.3, ..Default::default() }, Transform::identity(), None);
    }
    if let Some(dot) = PathBuilder::from_circle(left + r, top, r - 0.6) {
        p.fill_path(&dot, &fill, FillRule::Winding, Transform::identity(), None);
    }
    // Embaixo: pílula cheia com um furo redondo na ponta direita.
    let bottom = cy + 4.3;
    let mut pb = PathBuilder::new();
    let k = 0.5523 * r;
    let (x0, x1, y0, y1) = (left, left + w, bottom - r, bottom + r);
    pb.move_to(x0 + r, y0);
    pb.line_to(x1 - r, y0);
    pb.cubic_to(x1 - r + k, y0, x1, bottom - k, x1, bottom);
    pb.cubic_to(x1, bottom + k, x1 - r + k, y1, x1 - r, y1);
    pb.line_to(x0 + r, y1);
    pb.cubic_to(x0 + r - k, y1, x0, bottom + k, x0, bottom);
    pb.cubic_to(x0, bottom - k, x0 + r - k, y0, x0 + r, y0);
    pb.close();
    pb.push_circle(x1 - r, bottom, r - 1.4);
    if let Some(path) = pb.finish() {
        p.fill_path(&path, &fill, FillRule::EvenOdd, Transform::identity(), None);
    }
}

/// Engrenagem: anel com oito dentes.
fn draw_gear_icon(p: &mut Pixmap, cx: f32, cy: f32, fg: [u8; 4]) {
    let mut fill = Paint::default();
    fill.anti_alias = true;
    fill.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
    for i in 0..8 {
        let a = i as f32 * std::f32::consts::PI / 4.0;
        let mut pb = PathBuilder::new();
        let (s, c) = a.sin_cos();
        let (px, py) = (-s, c);
        let (r0, r1, half) = (5.0, 8.2, 1.6);
        pb.move_to(cx + c * r0 + px * half, cy + s * r0 + py * half);
        pb.line_to(cx + c * r1 + px * half, cy + s * r1 + py * half);
        pb.line_to(cx + c * r1 - px * half, cy + s * r1 - py * half);
        pb.line_to(cx + c * r0 - px * half, cy + s * r0 - py * half);
        pb.close();
        if let Some(path) = pb.finish() {
            p.fill_path(&path, &fill, FillRule::Winding, Transform::identity(), None);
        }
    }
    if let Some(ring) = PathBuilder::from_circle(cx, cy, 5.2) {
        p.stroke_path(&ring, &stroke_color(fg, 255), &Stroke { width: 2.4, ..Default::default() }, Transform::identity(), None);
    }
}

/// Lua (Não perturbe).
fn draw_moon_icon(p: &mut Pixmap, cx: f32, cy: f32, fg: [u8; 4]) {
    let mut pb = PathBuilder::new();
    pb.push_circle(cx, cy, 7.0);
    let Some(outer) = pb.finish() else {
        return;
    };
    let mut fill = Paint::default();
    fill.anti_alias = true;
    fill.set_color(rgba(fg[0], fg[1], fg[2], fg[3]));
    p.fill_path(&outer, &fill, FillRule::Winding, Transform::identity(), None);
    // O recorte da lua: um círculo apagado deslocado.
    if let Some(cut) = PathBuilder::from_circle(cx + 4.0, cy - 3.0, 6.0) {
        let mut clear = Paint::default();
        clear.anti_alias = true;
        clear.blend_mode = tiny_skia::BlendMode::DestinationOut;
        clear.set_color(rgba(0, 0, 0, 255));
        p.fill_path(&cut, &clear, FillRule::Winding, Transform::identity(), None);
    }
}

/// Interruptor no estilo do macOS: pílula azul (ligado) ou cinza, com a bolinha.
fn draw_switch(p: &mut Pixmap, x: f32, y: f32, on: bool) {
    let fill = if on { pal().accent_color() } else { pal().ink(40) };
    fill_rounded(p, x, y, 38.0, 22.0, 11.0, fill);
    let kx = if on { x + 27.0 } else { x + 11.0 };
    if let Some(knob) = PathBuilder::from_circle(kx, y + 11.0, 9.0) {
        let mut paint = Paint::default();
        paint.anti_alias = true;
        paint.set_color(rgba(255, 255, 255, 255));
        p.fill_path(&knob, &paint, FillRule::Winding, Transform::identity(), None);
    }
}

fn stroke_color(fg: [u8; 4], alpha: u8) -> Paint<'static> {
    let mut paint = Paint::default();
    paint.anti_alias = true;
    paint.set_color(rgba(fg[0], fg[1], fg[2], alpha));
    paint
}

/// Ícone de Wi-Fi: leque com até 3 arcos acesos (`level` 0 a 3; `None` = desligado).
fn draw_wifi_icon(p: &mut Pixmap, cx: f32, cy: f32, level: Option<u8>, fg: [u8; 4]) {
    let base = cy + 5.0;
    for i in 0..3u8 {
        let r = 4.5 + i as f32 * 3.6;
        let lit = level.is_some_and(|l| l > i);
        let alpha = if lit { 255 } else { 70 };
        let mut pb = PathBuilder::new();
        let a0 = std::f32::consts::PI * 1.25;
        let steps = 12;
        for k in 0..=steps {
            let a = a0 + (std::f32::consts::PI * 0.5) * k as f32 / steps as f32;
            let (px, py) = (cx + r * a.cos(), base + r * a.sin());
            if k == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        if let Some(path) = pb.finish() {
            let stroke = Stroke { width: 1.9, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
            p.stroke_path(&path, &stroke_color(fg, alpha), &stroke, Transform::identity(), None);
        }
    }
    if let Some(dot) = PathBuilder::from_circle(cx, base - 0.5, 1.6) {
        let alpha = if level.is_some() { 255 } else { 70 };
        p.fill_path(&dot, &stroke_color(fg, alpha), FillRule::Winding, Transform::identity(), None);
    }
    if level.is_none() {
        let mut pb = PathBuilder::new();
        pb.move_to(cx - 7.0, cy - 7.0);
        pb.line_to(cx + 7.0, cy + 7.0);
        if let Some(path) = pb.finish() {
            p.stroke_path(&path, &stroke_color(fg, 255), &Stroke { width: 1.6, ..Default::default() }, Transform::identity(), None);
        }
    }
}

/// Ícone do Bluetooth (a runa), mais apagado quando desligado.
fn draw_bluetooth_icon(p: &mut Pixmap, cx: f32, cy: f32, on: bool, fg: [u8; 4]) {
    let (h, w) = (7.0, 3.6);
    let mut pb = PathBuilder::new();
    pb.move_to(cx - w, cy - h * 0.45);
    pb.line_to(cx + w, cy + h * 0.45);
    pb.line_to(cx, cy + h);
    pb.line_to(cx, cy - h);
    pb.line_to(cx + w, cy - h * 0.45);
    pb.line_to(cx - w, cy + h * 0.45);
    if let Some(path) = pb.finish() {
        let stroke = Stroke { width: 1.6, line_join: tiny_skia::LineJoin::Round, line_cap: tiny_skia::LineCap::Round, ..Default::default() };
        p.stroke_path(&path, &stroke_color(fg, if on { 255 } else { 90 }), &stroke, Transform::identity(), None);
    }
}

/// Alto-falante com ondas pelo volume (0, 1, 2 ou 3), ou um × se estiver mudo.
fn draw_speaker_icon(p: &mut Pixmap, cx: f32, cy: f32, volume: Option<(f32, bool)>, fg: [u8; 4]) {
    let x = cx - 7.0;
    let mut pb = PathBuilder::new();
    pb.move_to(x, cy - 2.5);
    pb.line_to(x + 3.0, cy - 2.5);
    pb.line_to(x + 7.0, cy - 6.0);
    pb.line_to(x + 7.0, cy + 6.0);
    pb.line_to(x + 3.0, cy + 2.5);
    pb.line_to(x, cy + 2.5);
    pb.close();
    if let Some(path) = pb.finish() {
        p.fill_path(&path, &stroke_color(fg, 255), FillRule::Winding, Transform::identity(), None);
    }
    let (level, muted) = volume.unwrap_or((0.0, true));
    if muted {
        let mut pb = PathBuilder::new();
        pb.move_to(x + 10.0, cy - 3.0);
        pb.line_to(x + 16.0, cy + 3.0);
        pb.move_to(x + 16.0, cy - 3.0);
        pb.line_to(x + 10.0, cy + 3.0);
        if let Some(path) = pb.finish() {
            p.stroke_path(&path, &stroke_color(fg, 255), &Stroke { width: 1.6, line_cap: tiny_skia::LineCap::Round, ..Default::default() }, Transform::identity(), None);
        }
        return;
    }
    let waves = if level <= 0.01 { 0 } else if level < 0.34 { 1 } else if level < 0.67 { 2 } else { 3 };
    for i in 0..3 {
        let r = 3.5 + i as f32 * 3.0;
        let alpha = if i < waves { 255 } else { 60 };
        let mut pb = PathBuilder::new();
        let steps = 10;
        for k in 0..=steps {
            let a = -0.9 + 1.8 * k as f32 / steps as f32;
            let (px, py) = (x + 8.0 + r * a.cos(), cy + r * a.sin());
            if k == 0 {
                pb.move_to(px, py);
            } else {
                pb.line_to(px, py);
            }
        }
        if let Some(path) = pb.finish() {
            p.stroke_path(&path, &stroke_color(fg, alpha), &Stroke { width: 1.5, line_cap: tiny_skia::LineCap::Round, ..Default::default() }, Transform::identity(), None);
        }
    }
}

/// Rede cabeada: dois sinais de "maior e menor" com três pontos no meio.
fn draw_ethernet_icon(p: &mut Pixmap, cx: f32, cy: f32, fg: [u8; 4]) {
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 5.0, cy - 5.5);
    pb.line_to(cx - 8.5, cy);
    pb.line_to(cx - 5.0, cy + 5.5);
    pb.move_to(cx + 5.0, cy - 5.5);
    pb.line_to(cx + 8.5, cy);
    pb.line_to(cx + 5.0, cy + 5.5);
    if let Some(path) = pb.finish() {
        let stroke = Stroke { width: 1.7, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
        p.stroke_path(&path, &stroke_color(fg, 255), &stroke, Transform::identity(), None);
    }
    for dx in [-2.6, 0.0, 2.6] {
        if let Some(dot) = PathBuilder::from_circle(cx + dx, cy, 1.05) {
            p.fill_path(&dot, &stroke_color(fg, 255), FillRule::Winding, Transform::identity(), None);
        }
    }
}

/// Ícone de uma saída/entrada de som pelo tipo: fones, monitor, microfone, Bluetooth
/// ou alto-falante.
fn draw_audio_kind_icon(p: &mut Pixmap, cx: f32, cy: f32, kind: system::AudioKind, fg: [u8; 4]) {
    let line = Stroke { width: 1.6, line_cap: tiny_skia::LineCap::Round, line_join: tiny_skia::LineJoin::Round, ..Default::default() };
    match kind {
        system::AudioKind::Headphones | system::AudioKind::Headset => {
            // Arco por cima e as duas conchas.
            let mut pb = PathBuilder::new();
            pb.move_to(cx - 6.0, cy + 2.0);
            pb.line_to(cx - 6.0, cy - 0.5);
            pb.cubic_to(cx - 6.0, cy - 8.5, cx + 6.0, cy - 8.5, cx + 6.0, cy - 0.5);
            pb.line_to(cx + 6.0, cy + 2.0);
            if let Some(path) = pb.finish() {
                p.stroke_path(&path, &stroke_color(fg, 255), &line, Transform::identity(), None);
            }
            fill_rounded(p, cx - 7.5, cy + 0.5, 4.0, 6.5, 1.6, rgba(fg[0], fg[1], fg[2], 255));
            fill_rounded(p, cx + 3.5, cy + 0.5, 4.0, 6.5, 1.6, rgba(fg[0], fg[1], fg[2], 255));
            if kind == system::AudioKind::Headset {
                let mut pb = PathBuilder::new();
                pb.move_to(cx - 5.5, cy + 6.5);
                pb.quad_to(cx - 4.0, cy + 8.0, cx - 1.0, cy + 7.5);
                if let Some(path) = pb.finish() {
                    p.stroke_path(&path, &stroke_color(fg, 255), &Stroke { width: 1.2, ..line }, Transform::identity(), None);
                }
            }
        }
        system::AudioKind::Hdmi => {
            // Monitor com pé.
            if let Some(path) = rounded_rect(cx - 7.0, cy - 6.0, 14.0, 9.5, 1.5) {
                p.stroke_path(&path, &stroke_color(fg, 255), &Stroke { width: 1.5, ..Default::default() }, Transform::identity(), None);
            }
            fill_rounded(p, cx - 0.75, cy + 3.5, 1.5, 2.5, 0.5, rgba(fg[0], fg[1], fg[2], 255));
            fill_rounded(p, cx - 3.5, cy + 5.5, 7.0, 1.5, 0.75, rgba(fg[0], fg[1], fg[2], 255));
        }
        system::AudioKind::Mic => {
            fill_rounded(p, cx - 2.5, cy - 7.0, 5.0, 9.0, 2.5, rgba(fg[0], fg[1], fg[2], 255));
            let mut pb = PathBuilder::new();
            pb.move_to(cx - 5.0, cy - 1.0);
            pb.cubic_to(cx - 5.0, cy + 5.0, cx + 5.0, cy + 5.0, cx + 5.0, cy - 1.0);
            pb.move_to(cx, cy + 3.5);
            pb.line_to(cx, cy + 6.5);
            if let Some(path) = pb.finish() {
                p.stroke_path(&path, &stroke_color(fg, 255), &Stroke { width: 1.4, ..line }, Transform::identity(), None);
            }
        }
        system::AudioKind::Bluetooth => draw_bluetooth_icon(p, cx, cy, true, fg),
        _ => draw_speaker_icon(p, cx + 1.0, cy, Some((0.5, false)), fg),
    }
}

/// Cadeado pequeno (rede com senha).
fn draw_lock(p: &mut Pixmap, cx: f32, cy: f32, fg: [u8; 4]) {
    fill_rounded(p, cx - 4.5, cy - 1.5, 9.0, 7.0, 1.5, rgba(fg[0], fg[1], fg[2], 200));
    let mut pb = PathBuilder::new();
    pb.move_to(cx - 2.8, cy - 1.5);
    pb.line_to(cx - 2.8, cy - 4.0);
    pb.quad_to(cx - 2.8, cy - 6.8, cx, cy - 6.8);
    pb.quad_to(cx + 2.8, cy - 6.8, cx + 2.8, cy - 4.0);
    pb.line_to(cx + 2.8, cy - 1.5);
    if let Some(path) = pb.finish() {
        p.stroke_path(&path, &stroke_color(fg, 200), &Stroke { width: 1.4, ..Default::default() }, Transform::identity(), None);
    }
}

/// Ícone de bateria: contorno, ponta e preenchimento proporcional à carga.
fn draw_battery(pixmap: &mut Pixmap, x: f32, cy: f32, level: u8, charging: bool, fg: [u8; 4]) {
    let (w, h) = (24.0, 11.0);
    let y = cy - h / 2.0;
    if let Some(path) = rounded_rect(x + 0.5, y + 0.5, w, h, 3.0) {
        let mut paint = Paint::default();
        paint.set_color(rgba(fg[0], fg[1], fg[2], 200));
        paint.anti_alias = true;
        pixmap.stroke_path(&path, &paint, &Stroke::default(), Transform::identity(), None);
    }
    fill_rounded(pixmap, x + w + 1.5, cy - 2.0, 2.0, 4.0, 1.0, rgba(fg[0], fg[1], fg[2], 200));
    let fill_w = (w - 3.0) * (level.min(100) as f32 / 100.0);
    let color = if charging {
        rgba(40, 190, 70, 255)
    } else if level <= 20 {
        rgba(230, 60, 50, 255)
    } else {
        rgba(fg[0], fg[1], fg[2], 220)
    };
    fill_rounded(pixmap, x + 2.0, y + 2.0, fill_w, h - 3.0, 1.5, color);
}

/// Inicia um programa do mide que fica ao lado deste executável (ou no PATH).
fn spawn_sibling(name: &str) {
    let program = std::env::current_exe()
        .ok()
        .and_then(|exe| exe.parent().map(|dir| dir.join(name)))
        .filter(|path| path.exists())
        .unwrap_or_else(|| name.into());
    match Command::new(&program).spawn() {
        Ok(mut child) => {
            std::thread::spawn(move || {
                let _ = child.wait();
            });
            tracing::info!("abrindo {name}");
        }
        Err(err) => tracing::warn!("falha ao abrir {name}: {err}"),
    }
}

impl CompositorHandler for Bar {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: i32) {}
    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}
    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: u32) {}
    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for Bar {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.output_state
    }
    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}
}

impl LayerShellHandler for Bar {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        if layer == &self.layer {
            self.exit = true;
        } else {
            self.menu = None;
        }
    }

    fn configure(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        layer: &LayerSurface,
        configure: LayerSurfaceConfigure,
        _: u32,
    ) {
        if layer == &self.layer {
            self.width = configure.new_size.0;
            self.configured = true;
            self.draw_bar();
        } else if let Some(menu) = self.menu.as_mut() {
            menu.width = configure.new_size.0;
            menu.height = configure.new_size.1;
            self.draw_menu();
        }
    }
}

impl SeatHandler for Bar {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.seat_state
    }
    fn new_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer && self.pointer.is_none() {
            if let Ok(pointer) = self.seat_state.get_pointer(qh, &seat) {
                self.pointer = Some(pointer);
            }
        }
    }
    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        if capability == Capability::Pointer {
            if let Some(pointer) = self.pointer.take() {
                pointer.release();
            }
        }
    }
    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl PointerHandler for Bar {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for event in events {
            let (x, y) = event.position;
            let on_menu = self.menu.as_ref().is_some_and(|m| m.layer.wl_surface() == &event.surface);
            let on_bar = self.layer.wl_surface() == &event.surface;
            match event.kind {
                PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } if on_menu && self.menu.as_ref().is_some_and(|m| m.id.custom()) => {
                    self.custom_motion(x, y);
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT && on_menu && self.menu.as_ref().is_some_and(|m| m.id.custom()) => {
                    self.custom_press(x, y);
                }
                PointerEventKind::Release { button, .. } if button == BTN_LEFT && on_menu => {
                    if self.menu.as_mut().is_some_and(|m| std::mem::take(&mut m.dragging_brightness)) {
                        if let Some(value) = self.status.brightness {
                            system::set_brightness(value);
                        }
                    }
                    let dragged = self.menu.as_mut().map(|m| std::mem::take(&mut m.dragging_volume)).unwrap_or(false);
                    if dragged {
                        if let Some((value, _)) = self.status.volume {
                            system::set_volume(value);
                        }
                        // Toque de confirmação do volume novo, como no macOS.
                        mide::sounds::play("audio-volume-change");
                        self.request_status();
                    }
                }
                PointerEventKind::Motion { .. } | PointerEventKind::Enter { .. } if on_menu => {
                    // Na faixa da barra, passar por outro título troca de menu.
                    if y < BAR_HEIGHT as f64 {
                        if let Some(id) = self.title_at(x) {
                            if self.menu.as_ref().is_some_and(|m| m.id != id) {
                                self.open_menu(id);
                            }
                        }
                    }
                    let hovered = self.menu.as_ref().and_then(|m| m.item_at(x, y));
                    if self.menu.as_ref().is_some_and(|m| m.hovered != hovered) {
                        if let Some(menu) = self.menu.as_mut() {
                            menu.hovered = hovered;
                        }
                        self.draw_menu();
                    }
                }
                // Ícone do tray: clique esquerdo ativa o app (os que só têm menu abrem o
                // menu); o direito abre o menu.
                PointerEventKind::Press { button, .. }
                    if (button == BTN_LEFT || button == BTN_RIGHT) && on_bar && matches!(self.title_at(x), Some(MenuId::Tray(_))) =>
                {
                    let Some(MenuId::Tray(i)) = self.title_at(x) else {
                        continue;
                    };
                    let item = self.tray_items.get(i as usize).cloned();
                    match (self.tray.clone(), item) {
                        (Some(tray), Some(item)) if button == BTN_LEFT && !item.item_is_menu => {
                            self.close_menu();
                            tracing::info!("tray: ativar {}", item.id);
                            tray.activate(&item, x as i32, BAR_HEIGHT as i32);
                        }
                        _ => {
                            if self.menu.as_ref().is_some_and(|m| m.id == MenuId::Tray(i)) {
                                self.close_menu();
                            } else {
                                self.open_menu(MenuId::Tray(i));
                            }
                        }
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT && on_bar => {
                    if let Some(id) = self.title_at(x) {
                        if self.menu.as_ref().is_some_and(|m| m.id == id) {
                            self.close_menu();
                        } else {
                            self.open_menu(id);
                        }
                    }
                }
                PointerEventKind::Press { button, .. } if button == BTN_LEFT && on_menu => {
                    let item = self.menu.as_ref().and_then(|m| m.item_at(x, y).map(|i| m.items[i].clone()));
                    let inside = self.menu.as_ref().is_some_and(|m| m.inside_panel(x, y));
                    if let Some(MenuItem::Entry(_, _, Some(action))) = item {
                        self.run(action);
                    } else if y < BAR_HEIGHT as f64 && self.title_at(x).is_some() {
                        // Clicar no título do menu aberto fecha; em outro, troca.
                        let id = self.title_at(x);
                        if self.menu.as_ref().map(|m| m.id) == id {
                            self.close_menu();
                        } else if let Some(id) = id {
                            self.open_menu(id);
                        }
                    } else if !inside {
                        self.close_menu();
                    }
                }
                _ => {}
            }
        }
    }
}

impl ShmHandler for Bar {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.shm
    }
}

delegate_registry!(Bar);
smithay_client_toolkit::delegate_dispatch2!(Bar);

impl ProvidesRegistryState for Bar {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.registry_state
    }

    registry_handlers![OutputState, SeatState];
}

#[cfg(test)]
mod tests {
    use super::*;
    use chrono::TimeZone;

    #[test]
    fn relogio_mostra_so_o_escolhido() {
        let now = Local.with_ymd_and_hms(2026, 10, 21, 9, 5, 7).unwrap();
        assert_eq!(clock_format(now, false, false, false, false), "09:05");
        assert_eq!(clock_format(now, false, false, false, true), "09:05:07");
        assert_eq!(clock_format(now, true, true, false, false), "qua 21 out  09:05");
        assert_eq!(clock_format(now, true, true, true, false), "qua 21 out 2026  09:05");
    }
}
