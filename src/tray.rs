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
    let mut tray: Option<(TrayIcon, Option<Row>)> = None;
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
                if let Some((t, row)) = &tray {
                    if let Some(row) = row {
                        row.set_status(&text);
                    }
                    // Plain `set_icon` would drop the template flag on macOS.
                    let _ = t.set_icon_with_as_template(Some(icon(paired)), true);
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

fn build(items: &Items) -> Result<(TrayIcon, Option<Row>), Box<dyn std::error::Error>> {
    let place = Submenu::new("Other screen is", true);
    for p in &items.places {
        place.append(p)?;
    }
    let menu = Menu::new();
    // macOS shows the status as the subtitle of the on/off switch row;
    // elsewhere it gets a line of its own above a plain check item.
    if !cfg!(target_os = "macos") {
        menu.append_items(&[&items.status, &PredefinedMenuItem::separator()])?;
    }
    menu.append(&items.enabled)?;
    if cfg!(target_os = "macos") {
        menu.append(&PredefinedMenuItem::separator())?;
    }
    menu.append_items(&[&place, &items.login, &PredefinedMenuItem::separator(), &items.quit])?;
    #[cfg(target_os = "macos")]
    let row = switch_row::install(&menu, items.enabled.is_checked());
    #[cfg(not(target_os = "macos"))]
    let row = None;
    let tray = TrayIconBuilder::new()
        .with_menu(Box::new(menu))
        .with_icon(icon(false))
        .with_icon_as_template(true)
        .with_tooltip("mouser")
        .build()?;
    Ok((tray, row))
}

#[cfg(not(target_os = "macos"))]
struct Row;

#[cfg(not(target_os = "macos"))]
impl Row {
    fn set_status(&self, _: &str) {}
}

#[cfg(target_os = "macos")]
use switch_row::Row;

/// The Tailscale-style first row: "mouser" over the status, with a switch
/// on the right. muda has no switch item, so this puts an AppKit view on the
/// "Connection on" check item; the switch fires that item's own action, so
/// a flip arrives as the usual menu event and toggles the item's state.
#[cfg(target_os = "macos")]
mod switch_row {
    use objc2::rc::Retained;
    use objc2::{MainThreadMarker, sel};
    use objc2_app_kit::{
        NSAutoresizingMaskOptions, NSColor, NSControlStateValueOff, NSControlStateValueOn, NSFont,
        NSLineBreakMode, NSMenu, NSSwitch, NSTextField, NSView,
    };
    use objc2_foundation::{NSPoint, NSRect, NSSize, NSString};
    use tray_icon::menu::{ContextMenu, Menu};

    const WIDTH: f64 = 280.0;
    const HEIGHT: f64 = 42.0;
    /// Lines the text up with the titles of the items below (past the
    /// check mark column); the switch keeps the menu's usual right margin.
    const LEFT: f64 = 28.0;
    const RIGHT: f64 = 14.0;

    pub struct Row {
        subtitle: Retained<NSTextField>,
    }

    impl Row {
        pub fn set_status(&self, text: &str) {
            let text = NSString::from_str(text);
            self.subtitle.setStringValue(&text);
            self.subtitle.setToolTip(Some(&text)); // in case it is cut short
        }
    }

    fn rect(x: f64, y: f64, w: f64, h: f64) -> NSRect {
        NSRect::new(NSPoint::new(x, y), NSSize::new(w, h))
    }

    /// Turns the menu's first item into the switch row.
    pub fn install(menu: &Menu, on: bool) -> Option<Row> {
        let mtm = MainThreadMarker::new()?;
        // SAFETY: muda hands out its live NSMenu, which the tray keeps.
        let ns_menu = unsafe { menu.ns_menu().cast::<NSMenu>().as_ref() }?;
        let item = ns_menu.itemAtIndex(0)?;

        let view = NSView::initWithFrame(mtm.alloc(), rect(0.0, 0.0, WIDTH, HEIGHT));
        view.setAutoresizingMask(NSAutoresizingMaskOptions::ViewWidthSizable);

        let switch = NSSwitch::new(mtm);
        switch.setState(if on { NSControlStateValueOn } else { NSControlStateValueOff });
        switch.sizeToFit();
        let size = switch.frame().size;
        let switch_x = WIDTH - RIGHT - size.width;
        switch.setFrame(rect(switch_x, (HEIGHT - size.height) / 2.0, size.width, size.height));
        switch.setAutoresizingMask(NSAutoresizingMaskOptions::ViewMinXMargin);

        let text_width = switch_x - 8.0 - LEFT;
        let title = NSTextField::labelWithString(&NSString::from_str("mouser"), mtm);
        title.setFont(Some(&NSFont::menuFontOfSize(0.0)));
        title.setFrame(rect(LEFT, 20.0, text_width, 17.0));
        let subtitle = NSTextField::labelWithString(&NSString::from_str("Starting…"), mtm);
        subtitle.setFont(Some(&NSFont::systemFontOfSize(NSFont::smallSystemFontSize())));
        subtitle.setTextColor(Some(&NSColor::secondaryLabelColor()));
        subtitle.setLineBreakMode(NSLineBreakMode::ByTruncatingTail);
        subtitle.setFrame(rect(LEFT, 5.0, text_width, 15.0));
        // SAFETY: muda's item class implements this selector; the menu
        // retains the item for as long as the switch lives in it.
        unsafe {
            switch.setTarget(Some(&item));
            switch.setAction(Some(sel!(fireMenuItemAction:)));
        }

        view.addSubview(&title);
        view.addSubview(&subtitle);
        view.addSubview(&switch);
        item.setView(Some(&view));
        Some(Row { subtitle })
    }
}

/// The mouser mark: a computer mouse, drawn from distance fields so the
/// edges antialias. Outlined until paired, then filled with the wheel cut
/// out. 36 px tall, i.e. 18 pt at 2x, which is what the menu bar draws.
/// Black for the macOS template image (the system recolors it); white with
/// a dark rim on Windows so it reads on light and dark taskbars.
fn icon(paired: bool) -> Icon {
    const H: usize = 36;
    const W: usize = if cfg!(windows) { H } else { 30 };
    const STROKE: f32 = 3.0;
    let (cx, cy) = (W as f32 / 2.0, H as f32 / 2.0);
    // Body: a pill 18 x 28 px. Wheel: a short bar in the top half.
    let body = |x: f32, y: f32| {
        let (hw, hh, r) = (9.0, 14.0, 9.0);
        let qx = (x - cx).abs() - (hw - r);
        let qy = (y - cy).abs() - (hh - r);
        qx.max(0.0).hypot(qy.max(0.0)) + qx.max(qy).min(0.0) - r
    };
    let wheel = |x: f32, y: f32| {
        let dy = y - y.clamp(cy - 8.5, cy - 3.5);
        (x - cx).hypot(dy) - STROKE / 2.0
    };
    // Signed distance of the glyph: negative inside.
    let glyph = |x: f32, y: f32| {
        if paired {
            body(x, y).max(-(wheel(x, y) - 1.0))
        } else {
            ((body(x, y) + STROKE / 2.0).abs() - STROKE / 2.0).min(wheel(x, y))
        }
    };
    let cover = |d: f32| (0.5 - d).clamp(0.0, 1.0);
    let mut rgba = vec![0u8; W * H * 4];
    for y in 0..H {
        for x in 0..W {
            let (px, py) = (x as f32 + 0.5, y as f32 + 0.5);
            let a = cover(glyph(px, py));
            let out = &mut rgba[(y * W + x) * 4..][..4];
            if cfg!(windows) {
                // A 1.5 px dark rim around a white glyph.
                let rim = cover(glyph(px, py) - 1.5);
                let shade = (0x20 as f32 + (0xFF - 0x20) as f32 * a / rim.max(1e-3)) as u8;
                out.copy_from_slice(&[shade, shade, shade, (rim * 255.0) as u8]);
            } else {
                out[3] = (a * 255.0) as u8;
            }
        }
    }
    Icon::from_rgba(rgba, W as u32, H as u32).expect("RGBA icon")
}
