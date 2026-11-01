use gtk4::{
    gdk, glib, prelude::*, ApplicationWindow, Box as GtkBox, Button, GestureDrag, Image,
    Orientation, Overlay,
};
use gtk4_layer_shell::{Edge, KeyboardMode, Layer, LayerShell};
use niri_ipc::{socket::Socket, Action, PositionChange, Request, Response, SizeChange};
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::mpsc;
use std::time::Duration;

use crate::ssd::{find_monitor_by_connector, monitor_at_point, niri_action};

const GHOST_OFF: i32 = -1;
const CARD_DRAG_THRESHOLD: i32 = 4;

#[derive(Debug, Clone)]
pub struct CardSpec {
    pub window_id: u64,
    pub app_id: Option<String>,
    pub output: String,
    pub prev_x: i32,
    pub prev_y: i32,
    pub prev_w: i32,
    pub prev_h: i32,
    pub card_w: i32,
    pub card_h: i32,
}

struct CardHandle {
    win: ApplicationWindow,
    spec: CardSpec,
    output: Rc<RefCell<String>>,
}

struct Pos {
    mon: Option<gdk::Monitor>,
    top: i32,
    left: i32,
}

thread_local! {
    static CARDS: RefCell<HashMap<u64, CardHandle>> = RefCell::new(HashMap::new());
    static WATCHDOG: Cell<bool> = Cell::new(false);
}

pub fn is_card(id: u64) -> bool {
    CARDS.with(|c| c.borrow().contains_key(&id))
}

pub fn spawn_card(app: &gtk4::Application, spec: CardSpec) {
    if is_card(spec.window_id) {
        return;
    }
    ensure_watchdog();

    let id = spec.window_id;
    let (cw, ch) = (spec.card_w.max(1), spec.card_h.max(1));

    let win = ApplicationWindow::builder()
        .application(app)
        .title("cosSSDCard")
        .build();
    win.init_layer_shell();
    win.set_namespace(Some("shell-side-decorations-card"));
    win.set_layer(Layer::Overlay);
    win.set_keyboard_mode(KeyboardMode::None);
    win.remove_css_class("background");
    win.set_anchor(Edge::Top, true);
    win.set_anchor(Edge::Left, true);
    win.set_exclusive_zone(-1);

    let monitor = find_monitor_by_connector(&spec.output);
    if let Some(m) = &monitor {
        win.set_monitor(Some(m));
    }
    let top0 = (spec.prev_y - GHOST_OFF).max(0);
    let left0 = (spec.prev_x - GHOST_OFF).max(0);
    win.set_margin(Edge::Top, top0);
    win.set_margin(Edge::Left, left0);
    win.set_default_size(cw, ch);

    let root = GtkBox::new(Orientation::Vertical, 0);
    root.set_css_classes(&["ssdCard"]);
    root.set_size_request(cw, ch);
    root.set_cursor_from_name(Some("grab"));

    let icon = Image::from_icon_name(spec.app_id.as_deref().unwrap_or("application-x-executable"));
    icon.set_pixel_size(56);
    icon.set_halign(gtk4::Align::Center);
    icon.set_valign(gtk4::Align::Center);
    icon.set_hexpand(true);
    icon.set_vexpand(true);
    icon.set_css_classes(&["ssdDragGhostIcon"]);
    root.append(&icon);

    let restore_btn = Button::with_label("⤢");
    restore_btn.set_css_classes(&["ssdCardRestore"]);
    restore_btn.set_halign(gtk4::Align::End);
    restore_btn.set_valign(gtk4::Align::End);
    restore_btn.set_size_request(22, 22);
    restore_btn.set_tooltip_text(Some("Restore window"));
    restore_btn.set_cursor_from_name(Some("pointer"));

    let overlay = Overlay::new();
    overlay.set_child(Some(&root));
    overlay.add_overlay(&restore_btn);
    win.set_child(Some(&overlay));

    restore_btn.connect_clicked(move |_| restore_card(id));

    let pos = Rc::new(RefCell::new(Pos {
        mon: monitor,
        top: top0,
        left: left0,
    }));
    let output = Rc::new(RefCell::new(spec.output.clone()));
    let base: Rc<Cell<Option<(i32, i32)>>> = Rc::new(Cell::new(None));
    let moved = Rc::new(Cell::new(false));

    let drag = GestureDrag::new();

    drag.connect_drag_begin({
        let pos = pos.clone();
        let base = base.clone();
        let moved = moved.clone();
        let root = root.clone();
        move |_, _, _| {
            let p = pos.borrow();
            let (mx, my) = p
                .mon
                .as_ref()
                .map(|m| {
                    let g = m.geometry();
                    (g.x(), g.y())
                })
                .unwrap_or((0, 0));
            base.set(Some((mx + p.left, my + p.top)));
            moved.set(false);
            root.set_cursor_from_name(Some("grabbing"));
        }
    });

    drag.connect_drag_update({
        let pos = pos.clone();
        let base = base.clone();
        let moved = moved.clone();
        let win_weak = win.downgrade();
        move |_, dx, dy| {
            let Some((bx, by)) = base.get() else { return; };
            let Some(win) = win_weak.upgrade() else { return; };
            let (dx, dy) = (dx.round() as i32, dy.round() as i32);

            if !moved.get() {
                if dx.abs() < CARD_DRAG_THRESHOLD && dy.abs() < CARD_DRAG_THRESHOLD {
                    return;
                }
                moved.set(true);
            }

            let (nx, ny) = (bx + dx, by + dy);

            if let Some(target) = monitor_at_point(nx + cw / 2, ny + ch / 2) {
                let target_conn = target.connector().map(|c| c.to_string());
                let cur_conn = pos
                    .borrow()
                    .mon
                    .as_ref()
                    .and_then(|m| m.connector())
                    .map(|c| c.to_string());
                if target_conn != cur_conn {
                    win.set_visible(false);
                    win.set_monitor(Some(&target));
                    win.set_visible(true);
                    pos.borrow_mut().mon = Some(target);
                }
            }

            let mut p = pos.borrow_mut();
            let (mx, my) = p
                .mon
                .as_ref()
                .map(|m| {
                    let g = m.geometry();
                    (g.x(), g.y())
                })
                .unwrap_or((0, 0));
            p.top = ny - my;
            p.left = nx - mx;
            win.set_margin(Edge::Top, p.top);
            win.set_margin(Edge::Left, p.left);
        }
    });

    drag.connect_drag_end({
        let pos = pos.clone();
        let base = base.clone();
        let moved = moved.clone();
        let output = output.clone();
        let root = root.clone();
        move |_, _, _| {
            root.set_cursor_from_name(Some("grab"));
            let start = base.take();
            if !moved.replace(false) {
                return;
            }
            let Some((bx, by)) = start else { return; };

            let p = pos.borrow();
            let Some(mon) = p.mon.as_ref() else { return; };
            let g = mon.geometry();
            let end_conn = mon.connector().map(|c| c.to_string());

            let crossed = match &end_conn {
                Some(c) => *c != *output.borrow(),
                None => false,
            };

            if crossed {
                let c = end_conn.unwrap();
                niri_action(Action::MoveWindowToMonitor {
                    id: Some(id),
                    output: c.clone(),
                });
                niri_action(Action::MoveFloatingWindow {
                    id: Some(id),
                    x: PositionChange::SetFixed((p.left + GHOST_OFF) as f64),
                    y: PositionChange::SetFixed((p.top + GHOST_OFF) as f64),
                });
                *output.borrow_mut() = c;
            } else {
                let (ax, ay) = (g.x() + p.left, g.y() + p.top);
                let (ddx, ddy) = (ax - bx, ay - by);
                if ddx != 0 || ddy != 0 {
                    niri_action(Action::MoveFloatingWindow {
                        id: Some(id),
                        x: PositionChange::AdjustFixed(ddx as f64),
                        y: PositionChange::AdjustFixed(ddy as f64),
                    });
                }
            }
        }
    });

    root.add_controller(drag);

    niri_action(Action::SetWindowWidth {
        id: Some(id),
        change: SizeChange::SetFixed(cw),
    });
    niri_action(Action::SetWindowHeight {
        id: Some(id),
        change: SizeChange::SetFixed(ch),
    });

    win.set_visible(true);

    CARDS.with(|c| {
        c.borrow_mut().insert(
            id,
            CardHandle {
                win,
                spec,
                output,
            },
        )
    });
    publish_minimized();
}

fn publish_minimized() {
    let mut apps: Vec<String> = CARDS.with(|c| {
        c.borrow()
            .values()
            .map(|h| {
                h.spec
                    .app_id
                    .clone()
                    .unwrap_or_else(|| "unknown".to_string())
            })
            .collect()
    });
    apps.sort();
    crate::minicardstat::publish(apps);
}

pub fn restore_card(id: u64) {
    let Some(h) = CARDS.with(|c| c.borrow_mut().remove(&id)) else {
        return;
    };
    h.win.destroy();
    publish_minimized();

    let s = &h.spec;
    if *h.output.borrow() != s.output {
        niri_action(Action::MoveWindowToMonitor {
            id: Some(id),
            output: s.output.clone(),
        });
    }
    niri_action(Action::SetWindowWidth {
        id: Some(id),
        change: SizeChange::SetFixed(s.prev_w),
    });
    niri_action(Action::SetWindowHeight {
        id: Some(id),
        change: SizeChange::SetFixed(s.prev_h),
    });
    niri_action(Action::MoveFloatingWindow {
        id: Some(id),
        x: PositionChange::SetFixed(s.prev_x as f64),
        y: PositionChange::SetFixed(s.prev_y as f64),
    });
    niri_action(Action::FocusWindow { id });
}

pub fn maximize_app(app_id: &str) -> usize {
    let ids: Vec<u64> = CARDS.with(|c| {
        c.borrow()
            .iter()
            .filter(|(_, h)| {
                h.spec
                    .app_id
                    .as_deref()
                    .map_or(false, |a| a.eq_ignore_ascii_case(app_id))
            })
            .map(|(id, _)| *id)
            .collect()
    });
    for id in &ids {
        restore_card(*id);
    }
    ids.len()
}

/// Drops cards whose window was closed by the app itself.
fn ensure_watchdog() {
    if WATCHDOG.with(|w| w.replace(true)) {
        return;
    }
    let (tx, rx) = mpsc::channel::<Vec<u64>>();
    std::thread::spawn(move || loop {
        std::thread::sleep(Duration::from_secs(1));
        let Ok(mut sock) = Socket::connect() else { continue; };
        if let Ok(Ok(Response::Windows(ws))) = sock.send(Request::Windows) {
            if tx.send(ws.iter().map(|w| w.id).collect()).is_err() {
                return;
            }
        }
    });

    glib::timeout_add_local(Duration::from_millis(500), move || {
        let mut last = None;
        while let Ok(v) = rx.try_recv() {
            last = Some(v);
        }
        if let Some(live) = last {
            let gone: Vec<u64> = CARDS.with(|c| {
                c.borrow()
                    .keys()
                    .copied()
                    .filter(|id| !live.contains(id))
                    .collect()
            });
            let changed = !gone.is_empty();
            for id in gone {
                if let Some(h) = CARDS.with(|c| c.borrow_mut().remove(&id)) {
                    h.win.destroy();
                }
            }
            if changed {
                publish_minimized();
            }
        }
        glib::ControlFlow::Continue
    });
}