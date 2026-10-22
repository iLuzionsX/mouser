//! The menu bar (macOS) / notification area (Windows) icon and its menu:
//! status, on/off, where the other screen is, start at login, quit. It owns
//! the main thread, which macOS requires for anything UI.

use std::sync::mpsc::Sender;

use tao::event::{Event, StartCause};
use tao::event_loop::{ControlFlow, EventLoopBuilder, EventLoopProxy};
use tray_icon::menu::{CheckMenuItem, Menu, MenuEvent, MenuItem, PredefinedMenuItem, Submenu};
use tray_icon::{Icon, TrayIcon, TrayIconBuilder};

use crate::Ev;
use crate::autostart;
use crate::config::Config;
use crate::kvm::Edge;

/// The menu's words for each edge, in `Edge::ALL` order.
const PLACES: [&str; 4] = ["Left", "Right", "Above", "Below"];

pub enum Ui {
    Menu(MenuEvent),
    Status(String, bool),
}

/// Lets the worker thread update the status line and icon.
#[derive(Clone)]
pub struct Status(EventLoopProxy<Ui>);

impl Status {
    pub fn set(&self, text: impl Into<String>, paired: bool) {
        let _ = self.0.send_event(Ui::Status(text.into(), paired));
    }
}

struct Items {
    status: MenuItem,
    enabled: CheckMenuItem,
    places: Vec<CheckMenuItem>,
    login: CheckMenuItem,
    quit: MenuItem,
}

/// Runs the UI on this (main) thread forever; `start` gets the status handle
/// once the loop is up.
pub fn run(cfg: &Config, tx: Sender<Ev>, start: impl FnOnce(Status) + 'static) -> ! {
    #[allow(unused_mut)]
    let mut events = EventLoopBuilder::<Ui>::with_user_event().build();
    #[cfg(target_os = "macos")]
    {
        use tao::platform::macos::{ActivationPolicy, EventLoopExtMacOS};
        events.set_activation_policy(ActivationPolicy::Accessory); // no Dock icon
    }
    let proxy = events.create_proxy();
    let menu_proxy = proxy.clone();
    MenuEvent::set_event_handler(Some(move |e| {
        let _ = menu_proxy.send_event(Ui::Menu(e));
    }));

    let items = Items {
        status: MenuItem::new("Starting…", false, None),
        enabled: CheckMenuItem::new("Connection on", true, cfg.enabled, None),
        places: Edge::ALL
            .iter()
            .zip(PLACES)
            .map(|(&e, name)| CheckMenuItem::new(name, true, cfg.edge == Some(e), None))
            .collect(),
        login: CheckMenuItem::new("Start at login", true, autostart::enabled(), None),
        quit: MenuItem::new("Quit mouser", true, None),
    };
    let mut start = Some(start);
    let mut tray: Option<TrayIcon> = None;
    events.run(move |event, _, flow| {
        *flow = ControlFlow::Wait;
        match event {
            // macOS wants the status item created once the loop runs.
            Event::NewEvents(StartCause::Init) => {
                tray = build(&items).map_err(|e| log!("no tray icon: {e}")).ok();
                if let Some(start) = start.take() {
                    start(Status(proxy.clone()));
                }
            }
            Event::UserEvent(Ui::Status(text, paired)) => {
                items.status.set_text(&text);
                if let Some(t) = &tray {
                    let _ = t.set_icon(Some(icon(paired)));
                    let _ = t.set_tooltip(Some(format!("mouser: {text}")));
                }
            }
            Event::UserEvent(Ui::Menu(e)) => {
                let id = e.id();
                if id == items.enabled.id() {
                    let _ = tx.send(Ev::Enable(items.enabled.is_checked()));
                } else if id == items.login.id() {
                    let on = items.login.is_checked();
                    if let Err(e) = autostart::set(on) {
                        log!("could not change start at login: {e}");
                    }
                    items.login.set_checked(autostart::enabled());
                } else if id == items.quit.id() {
                    let _ = tx.send(Ev::Quit);
                } else if let Some(i) = items.places.iter().position(|p| p.id() == id) {
                    // Check items toggle themselves; make these act as radios.
                    for (j, p) in items.places.iter().enumerate() {
                        p.set_checked(i == j);
                    }
                    let _ = tx.send(Ev::Edge(Edge::ALL[i]));
                }
            }
            _ => {}
        }
    })
}

fn build(items: &Items) -> Result<TrayIcon, Box<dyn std::error::Error>> {
    let place = Submenu::new("Other screen is", true);
    for p in &items.places {
        place.append(p)?;
    }
    let menu = Menu::new();
    menu.append_items(&[
        &items.status,
        &PredefinedMenuItem::separator(),
        &items.enabled,
        &place,
        &items.login,
        &PredefinedMenuItem::separator(),
        &items.quit,
    ])?;
    Ok(TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon(false))
        .with_icon_as_template(true)
        .with_tooltip("mouser")
        .build()?)
}

/// Two stacked screens, outlined until paired, then filled. Black for the
/// macOS template image (the system recolors it); white with a dark rim
/// on Windows so it reads on light and dark taskbars.
fn icon(paired: bool) -> Icon {
    const N: usize = 32;
    let mut rgba = vec![0u8; N * N * 4];
    let screen = |x: usize, y: usize| {
        (4..28).contains(&x) && ((3..14).contains(&y) || (18..29).contains(&y))
    };
    let rim = |x: usize, y: usize| {
        screen(x, y)
            && !(screen(x.wrapping_sub(2), y)
                && screen(x + 2, y)
                && screen(x, y.wrapping_sub(2))
                && screen(x, y + 2))
    };
    for y in 0..N {
        for x in 0..N {
            let px = &mut rgba[(y * N + x) * 4..][..4];
            let on = if paired { screen(x, y) } else { rim(x, y) };
            if cfg!(windows) {
                let edge = screen(x, y)
                    && !(screen(x.wrapping_sub(1), y)
                        && screen(x + 1, y)
                        && screen(x, y.wrapping_sub(1))
                        && screen(x, y + 1));
                if edge {
                    px.copy_from_slice(&[0x20, 0x20, 0x20, 0xFF]);
                } else if on {
                    px.copy_from_slice(&[0xFF, 0xFF, 0xFF, 0xFF]);
                }
            } else if on {
                px[3] = 0xFF;
            }
        }
    }
    Icon::from_rgba(rgba, N as u32, N as u32).expect("32x32 RGBA")
}
