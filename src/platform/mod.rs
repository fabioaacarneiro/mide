//! Sistema e sessão: estado e ações do sistema (som, rede, energia), informações da máquina, telas, teclado, aparência, configuração, itens de início, senha de administrador, portal e canal de controle do compositor.

pub mod system;
pub mod audio;
pub mod network;
pub mod sysinfo;
pub mod displays;
pub mod keyboard_layouts;
pub mod appearance;
pub mod login_items;
pub mod elevate;
pub mod portal;
pub mod mpris;

// No framework mide-ui (github.com/fabioaacarneiro/mide-ui), repassados aqui.
pub use mide_ui::platform::{control_client, settings_file, sounds};
