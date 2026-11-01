//! Small native tray foundation for the Slint app.
//!
//! The tray is deliberately target-scoped: Windows is the supported desktop
//! target, while non-Windows builds keep the core crate free of GTK/system
//! tray dependencies. Menu events are polled by the Slint timer because the
//! Slint 1.9 public API does not expose a portable tray-event hook.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Command {
    ShowBoard,
    CreateNote,
    Quit,
}

#[cfg(windows)]
mod platform {
    use super::Command;
    use tray_icon::{
        menu::{Menu, MenuEvent, MenuItem},
        Icon, TrayIcon, TrayIconBuilder,
    };

    const SHOW_BOARD: &str = "sticky-notes-show-board";
    const CREATE_NOTE: &str = "sticky-notes-create-note";
    const QUIT: &str = "sticky-notes-quit";

    pub struct Tray {
        // Keeping this handle alive keeps the icon registered for the process.
        _icon: TrayIcon,
    }

    impl Tray {
        pub fn create() -> anyhow::Result<Self> {
            let show_board = MenuItem::with_id(SHOW_BOARD, "Show board", true, None);
            let create_note = MenuItem::with_id(CREATE_NOTE, "New sticky", true, None);
            let quit = MenuItem::with_id(QUIT, "Quit", true, None);
            let menu = Menu::with_items(&[&show_board, &create_note, &quit])?;
            let picture =
                image::load_from_memory(include_bytes!("../ui/icons/tray-icon.png"))?.into_rgba8();
            let (width, height) = picture.dimensions();
            let icon = Icon::from_rgba(picture.into_raw(), width, height)?;
            let icon = TrayIconBuilder::new()
                .with_menu(Box::new(menu))
                .with_tooltip("Sticky Notes+")
                .with_icon(icon)
                .build()?;
            Ok(Self { _icon: icon })
        }

        pub fn poll(&self) -> Option<Command> {
            let event = MenuEvent::receiver().try_iter().last()?;
            match event.id.0.as_str() {
                SHOW_BOARD => Some(Command::ShowBoard),
                CREATE_NOTE => Some(Command::CreateNote),
                QUIT => Some(Command::Quit),
                _ => None,
            }
        }
    }
}

#[cfg(windows)]
pub use platform::Tray;

#[cfg(not(windows))]
pub struct Tray;

#[cfg(not(windows))]
impl Tray {
    pub fn create() -> anyhow::Result<Self> {
        Ok(Self)
    }

    pub fn poll(&self) -> Option<Command> {
        None
    }
}
