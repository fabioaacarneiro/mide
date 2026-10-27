//! Biblioteca do mide, usada pelos programas do mide (barra, dock, apps) e pelo
//! compositor (`src/compositor/`).
//!
//! Organização por assunto:
//! - `kit` e `ui`: o mide-ui (framework de apps), projeto à parte (github.com/fabioaacarneiro/mide-ui);
//! - `platform`: sistema e sessão;
//! - `shell`: peças da barra, dock, launcher, busca e notificações;
//! - `applications`: apps instalados e como abrir coisas;
//! - `storage`: arquivos, pastas e miniaturas;
//! - `documents`: texto formatado, ortografia e marcações;
//! - `media`: o núcleo do player de vídeo;
//! - `terminal` e `calculator`: os núcleos do Terminal e da Calculadora.
//!
//! Os módulos também ficam na raiz (`mide::sounds`, `mide::files`...), para os
//! caminhos curtos continuarem valendo em todo o código.

pub mod applications;
pub mod calculator;
pub mod code;
pub mod documents;
pub mod media;
pub mod platform;
pub mod shell;
pub mod storage;
pub mod terminal;

// O mide-ui (framework de apps, github.com/fabioaacarneiro/mide-ui) e o desenho de baixo nível.
pub use mide_ui::{kit, ui};

pub use applications::{appimage, apps, default_apps, icons, programs, xpm};
pub use documents::{annotate, markup, richtext, spell};
pub use platform::{appearance, audio, network, control_client, displays, elevate, keyboard_layouts, login_items, portal, mpris, settings_file, sounds, sysinfo, system};
pub use shell::{appmenu, backdrop, calendar, gmenu, launcher_layout, motion, notify, search, tray};
pub use storage::{files, files_info, files_view, places, templates, thumbnails};
