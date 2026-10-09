//! System tray icon: click to show the window, menu for playback and quitting.

pub enum TrayCmd {
    Show,
    TogglePlay,
    Prev,
    Next,
    Quit,
}

#[cfg(windows)]
mod imp {
    use super::TrayCmd;
    use tray_icon::menu::{Menu, MenuEvent, MenuId, MenuItem, PredefinedMenuItem};
    use tray_icon::{
        Icon, MouseButton, MouseButtonState, TrayIcon, TrayIconBuilder, TrayIconEvent,
    };

    pub struct Tray {
        icon: TrayIcon,
        show: MenuId,
        play: MenuId,
        prev: MenuId,
        next: MenuId,
        quit: MenuId,
        tooltip: String,
        /// show, play, prev, next, quit (kept to relabel them when the language changes)
        items: Vec<MenuItem>,
    }

    fn labels() -> [String; 5] {
        [
            crate::tr!("顯示 Simple Player", "Show Simple Player"),
            crate::tr!("播放 / 暫停", "Play / Pause"),
            crate::tr!("上一首", "Previous"),
            crate::tr!("下一首", "Next"),
            crate::tr!("結束", "Quit"),
        ]
    }

    impl Tray {
        pub fn new() -> Option<Tray> {
            let menu = Menu::new();
            let [l_show, l_play, l_prev, l_next, l_quit] = labels();
            let show = MenuItem::new(l_show, true, None);
            let play = MenuItem::new(l_play, true, None);
            let prev = MenuItem::new(l_prev, true, None);
            let next = MenuItem::new(l_next, true, None);
            let quit = MenuItem::new(l_quit, true, None);
            menu.append_items(&[
                &show,
                &PredefinedMenuItem::separator(),
                &play,
                &prev,
                &next,
                &PredefinedMenuItem::separator(),
                &quit,
            ])
            .ok()?;
            // Icon id 1 = the .exe icon (build.rs); ask for the system's small-icon size so
            // Windows picks the hand-tuned 16/20/24 px layer instead of shrinking a big one.
            #[link(name = "user32")]
            extern "system" {
                fn GetSystemMetrics(index: i32) -> i32;
            }
            const SM_CXSMICON: i32 = 49;
            let px = unsafe { GetSystemMetrics(SM_CXSMICON) }.max(16) as u32;
            let icon = Icon::from_resource(1, Some((px, px))).ok()?;
            let tray = TrayIconBuilder::new()
                .with_icon(icon)
                .with_tooltip("Simple Player")
                .with_menu(Box::new(menu))
                .with_menu_on_left_click(false)
                .build()
                .ok()?;
            Some(Tray {
                icon: tray,
                show: show.id().clone(),
                play: play.id().clone(),
                prev: prev.id().clone(),
                next: next.id().clone(),
                quit: quit.id().clone(),
                tooltip: String::new(),
                items: vec![show, play, prev, next, quit],
            })
        }

        /// Pending clicks and menu choices (call from a UI timer).
        pub fn poll(&self) -> Vec<TrayCmd> {
            let mut out = Vec::new();
            while let Ok(ev) = TrayIconEvent::receiver().try_recv() {
                match ev {
                    TrayIconEvent::Click {
                        button: MouseButton::Left,
                        button_state: MouseButtonState::Up,
                        ..
                    } => out.push(TrayCmd::Show),
                    _ => {}
                }
            }
            while let Ok(ev) = MenuEvent::receiver().try_recv() {
                let id = ev.id;
                if id == self.show {
                    out.push(TrayCmd::Show);
                } else if id == self.play {
                    out.push(TrayCmd::TogglePlay);
                } else if id == self.prev {
                    out.push(TrayCmd::Prev);
                } else if id == self.next {
                    out.push(TrayCmd::Next);
                } else if id == self.quit {
                    out.push(TrayCmd::Quit);
                }
            }
            out
        }

        pub fn set_tooltip(&mut self, text: &str) {
            if self.tooltip != text {
                self.tooltip = text.to_string();
                let _ = self.icon.set_tooltip(Some(text));
            }
        }

        /// Re-reads the menu texts (after a language change).
        pub fn relabel(&self) {
            for (item, label) in self.items.iter().zip(labels()) {
                item.set_text(label);
            }
        }
    }
}

#[cfg(not(windows))]
mod imp {
    use super::TrayCmd;
    pub struct Tray;
    impl Tray {
        pub fn new() -> Option<Tray> {
            None
        }
        pub fn poll(&self) -> Vec<TrayCmd> {
            Vec::new()
        }
        pub fn set_tooltip(&mut self, _: &str) {}
        pub fn relabel(&self) {}
    }
}

pub use imp::Tray;
