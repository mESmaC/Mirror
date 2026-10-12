//! Native window management and composition. Applications own their content;
//! this process owns only window geometry, decoration, focus and scanout.

use crate::render::{self, Rect, Renderer, WindowView};
use crate::{
    Event as HardwareEvent, SYS_INPUT_POLL, SYS_SLEEP, SYS_TICKS, boot_handle, exit, next_input,
    require, surface_dimensions, syscall, write,
};
use mirror_ui_tokens::{DEFAULT, Role};
use mirror_window_protocol::{self as protocol, AppInfo, Event, TileRequest};

const OUTER_WIDTH: usize = protocol::WIDTH + 2;
const OUTER_HEIGHT: usize = protocol::HEIGHT + 30;
const TASKBAR_HEIGHT: usize = 48;
const PENDING_EVENTS: usize = 16;
const LOG_CAPACITY: usize = 32;
// Type roles on the 5x7 font (0029-D10); every existing string keeps its
// scale and position. The other strings name their roles where drawn.
const WORDMARK: Role = Role::Title3;
const STOPPED_TITLE: Role = Role::Title2;
// App pixels the smoke oracles wait for (0029-D6). Notes and Counter draw the
// same tokens, so slots, not colors, identify windows.
const FOCUSED_BORDER: [u8; 4] = DEFAULT.accent.rgbx();
const UNFOCUSED_BORDER: [u8; 4] = DEFAULT.border_strong.rgbx();
const INK: [u8; 4] = DEFAULT.text_primary.rgbx();

#[derive(Clone, Copy)]
struct Pending {
    events: [Option<Event>; PENDING_EVENTS],
    length: usize,
}

impl Pending {
    const fn new() -> Self {
        Self {
            events: [None; PENDING_EVENTS],
            length: 0,
        }
    }

    fn push(&mut self, event: Event, focused: bool) {
        if let Event::Pointer { buttons, .. } = event
            && self.length != 0
            && let Some(Event::Pointer {
                buttons: previous, ..
            }) = self.events[self.length - 1]
            && buttons == previous
        {
            self.events[self.length - 1] = Some(event);
            return;
        }
        if self.length == PENDING_EVENTS {
            // A lost edge cannot leave an app believing a button remains held.
            // Discard this burst and establish a fresh focus/input baseline.
            self.events.fill(None);
            self.events[0] = Some(Event::Reset);
            let focused = if let Event::Focus(value) = event {
                value
            } else {
                focused
            };
            self.events[1] = Some(Event::Focus(focused));
            self.length = 2;
            return;
        }
        self.events[self.length] = Some(event);
        self.length += 1;
    }

    fn pop(&mut self) {
        self.events.copy_within(1..self.length, 0);
        self.length -= 1;
        self.events[self.length] = None;
    }
}

#[derive(Clone, Copy)]
struct Window {
    handle: u64,
    pid: u64,
    kind: u64,
    x: usize,
    y: usize,
    minimized: bool,
    state: u32,
    revision: u64,
    ready: bool,
    pending: Pending,
}

impl Window {
    fn outer(self) -> Rect {
        Rect::new(self.x, self.y, OUTER_WIDTH, OUTER_HEIGHT)
    }
    /// A window draws nothing outside its outer rectangle: the offset shadow
    /// block is gone (0029-D4), so its damage no longer reaches 8 px beyond.
    fn damage(self) -> Rect {
        self.outer()
    }
    fn content(self) -> Rect {
        Rect::new(self.x + 1, self.y + 29, protocol::WIDTH, protocol::HEIGHT)
    }
    fn close_button(self) -> Rect {
        Rect::new(self.x + 293, self.y + 5, 22, 18)
    }
    fn minimize_button(self) -> Rect {
        Rect::new(self.x + 266, self.y + 5, 22, 18)
    }
    /// Shown only while the app is stopped (0029-D8). The spec places it at
    /// content-local (23, 100), below "Relaunch it, or close this window.";
    /// in today's chrome the content starts at y + 29.
    fn relaunch_button(self) -> Rect {
        Rect::new(self.x + 24, self.y + 129, 96, 28)
    }
    fn stopped(self) -> bool {
        self.state != protocol::RUNNING
    }
}

/// Why the last launch was refused. While one is set, a toast says so above
/// the windows (0029-D8); the next launch or close clears it.
#[derive(Clone, Copy)]
enum Refusal {
    /// Both window slots are in use.
    NoSlot,
    /// `SYS_APP_LAUNCH` failed.
    LaunchFailed,
}

impl Refusal {
    /// The `refused launch` capture fence (trace builds only, 0010-D4).
    fn code(self) -> u64 {
        match self {
            Self::NoSlot => 1,
            Self::LaunchFailed => 2,
        }
    }
}

#[derive(Clone, Copy)]
enum Capture {
    None,
    Drag { slot: usize, x: usize, y: usize },
    Client(usize),
}

#[derive(Clone, Copy)]
struct Log {
    label: &'static [u8],
    values: [u64; 3],
    length: usize,
}

struct Shell {
    width: usize,
    height: usize,
    supervisor: u64,
    windows: [Option<Window>; protocol::MAX_APPS],
    order: [usize; protocol::MAX_APPS],
    focused: Option<usize>,
    pointer_x: usize,
    pointer_y: usize,
    buttons: u8,
    clicks_armed: bool,
    capture: Capture,
    scene_dirty: bool,
    damaged: Option<Rect>,
    logs: [Option<Log>; LOG_CAPACITY],
    log_length: usize,
    pointer_changed: bool,
    launch_failed: Option<Refusal>,
}

impl Shell {
    fn new(width: usize, height: usize, supervisor: u64) -> Self {
        require(width >= 480 && height >= 360, 140);
        Self {
            width,
            height,
            supervisor,
            windows: [None; protocol::MAX_APPS],
            order: [0, 1],
            focused: None,
            pointer_x: width / 2,
            pointer_y: height / 2,
            buttons: 0,
            clicks_armed: false,
            capture: Capture::None,
            scene_dirty: true,
            damaged: Some(Rect::new(0, 0, width, height)),
            logs: [None; LOG_CAPACITY],
            log_length: 0,
            pointer_changed: false,
            launch_failed: None,
        }
    }

    fn log(&mut self, label: &'static [u8], values: &[u64]) {
        if self.log_length == LOG_CAPACITY {
            return;
        }
        let mut entry = Log {
            label,
            values: [0; 3],
            length: values.len(),
        };
        entry.values[..values.len()].copy_from_slice(values);
        self.logs[self.log_length] = Some(entry);
        self.log_length += 1;
    }

    fn flush_logs(&mut self, console: u64) {
        for entry in self.logs[..self.log_length].iter_mut() {
            let log = entry.take().unwrap();
            log_values(console, log.label, &log.values[..log.length]);
        }
        self.log_length = 0;
        if self.pointer_changed {
            log_values(
                console,
                b"pointer",
                &[
                    self.pointer_x as u64,
                    self.pointer_y as u64,
                    u64::from(self.buttons),
                ],
            );
            self.pointer_changed = false;
        }
    }

    fn invalidate(&mut self, rect: Rect) {
        self.scene_dirty = true;
        self.damaged = Some(match self.damaged {
            None => rect,
            Some(old) => {
                let x = old.x.min(rect.x);
                let y = old.y.min(rect.y);
                Rect::new(
                    x,
                    y,
                    (old.x + old.width).max(rect.x + rect.width) - x,
                    (old.y + old.height).max(rect.y + rect.height) - y,
                )
            }
        });
    }

    fn taskbar(&self) -> Rect {
        Rect::new(0, self.height - TASKBAR_HEIGHT, self.width, TASKBAR_HEIGHT)
    }
    fn launcher(&self, kind: u64) -> Rect {
        Rect::new(
            if kind == protocol::NOTES { 20 } else { 116 },
            self.height - 40,
            if kind == protocol::NOTES { 88 } else { 100 },
            32,
        )
    }
    fn task_button(&self, slot: usize) -> Rect {
        // Two task buttons also fit the 480-pixel minimum mode.
        let width = ((self.width - 252) / protocol::MAX_APPS).min(156);
        Rect::new(236 + slot * (width + 8), self.height - 40, width, 32)
    }
    /// The refused-launch toast, top right below the top bar (0029-D8). It
    /// depends on no window geometry and fits the 480-pixel minimum width.
    fn toast(&self) -> Rect {
        Rect::new(self.width - 326, 44, 310, 52)
    }

    fn refuse_launch(&mut self, refusal: Refusal) {
        self.launch_failed = Some(refusal);
        // The launchers no longer change on a refusal; only the toast does.
        self.invalidate(self.toast());
        self.log(b"refused launch", &[refusal.code()]);
    }

    fn clear_launch_failure(&mut self) {
        if self.launch_failed.take().is_some() {
            self.invalidate(self.toast());
        }
    }

    fn enqueue(&mut self, slot: usize, event: Event) {
        let focused = self.focused == Some(slot);
        if let Some(window) = &mut self.windows[slot]
            && window.state == protocol::RUNNING
        {
            window.pending.push(event, focused);
        }
    }

    fn focus(&mut self, slot: Option<usize>) {
        let previous = self.focused;
        let changed = previous != slot;
        self.focused = slot;
        if let Some(old) = previous
            && Some(old) != slot
        {
            self.enqueue(old, Event::Focus(false));
            self.invalidate(self.windows[old].unwrap().damage());
        }
        if let Some(slot) = slot {
            if changed {
                self.enqueue(slot, Event::Focus(true));
            }
            let index = self.order.iter().position(|entry| *entry == slot).unwrap();
            self.order[index..].rotate_left(1);
            self.invalidate(self.windows[slot].unwrap().damage());
            if changed {
                self.log(b"focus", &[slot as u64]);
            }
        }
        self.invalidate(self.taskbar());
    }

    fn top_visible(&self) -> Option<usize> {
        self.order
            .iter()
            .rev()
            .copied()
            .find(|&slot| self.windows[slot].is_some_and(|w| !w.minimized))
    }

    fn launch(&mut self, kind: u64) -> Option<usize> {
        let Some(slot) = self.windows.iter().position(Option::is_none) else {
            self.refuse_launch(Refusal::NoSlot);
            return None;
        };
        let handle = syscall(protocol::SYS_APP_LAUNCH, self.supervisor, kind, 0);
        if handle <= 0 {
            self.refuse_launch(Refusal::LaunchFailed);
            return None;
        }
        let info = app_info(handle as u64).unwrap_or_else(|| exit(141));
        let x = (80 + slot * 350).min(self.width - OUTER_WIDTH - 12);
        let y = (100 + slot * 60)
            .min(self.height - TASKBAR_HEIGHT - OUTER_HEIGHT - 8)
            .max(44);
        self.windows[slot] = Some(Window {
            handle: handle as u64,
            pid: info.pid,
            kind,
            x,
            y,
            minimized: false,
            state: info.state,
            revision: 0,
            ready: false,
            pending: Pending::new(),
        });
        self.clear_launch_failure();
        self.log(b"launch", &[slot as u64, info.pid, kind]);
        self.focus(Some(slot));
        Some(slot)
    }

    fn close(&mut self, slot: usize) {
        let Some(window) = self.windows[slot] else {
            return;
        };
        require(
            syscall(protocol::SYS_APP_CLOSE, window.handle, 0, 0) == 0,
            142,
        );
        if self.focused == Some(slot) {
            self.focused = None;
        }
        self.windows[slot] = None;
        self.capture = Capture::None;
        self.clear_launch_failure();
        self.invalidate(window.damage());
        self.log(b"close", &[slot as u64, window.pid]);
        self.focus(self.top_visible());
    }

    /// The Relaunch button of a stopped window (0029-D8): close it, then
    /// launch the same kind, which takes the slot just freed.
    fn relaunch(&mut self, slot: usize) {
        let Some(window) = self.windows[slot] else {
            return;
        };
        self.close(slot);
        self.launch(window.kind);
    }

    fn minimize(&mut self, slot: usize) {
        let mut window = self.windows[slot].unwrap();
        window.minimized = true;
        self.windows[slot] = Some(window);
        self.invalidate(window.damage());
        self.log(b"minimize", &[slot as u64]);
        self.focus(self.top_visible());
    }

    fn restore(&mut self, slot: usize) {
        let mut window = self.windows[slot].unwrap();
        if window.minimized {
            window.minimized = false;
            self.windows[slot] = Some(window);
            self.log(b"restore", &[slot as u64]);
        }
        self.focus(Some(slot));
    }

    fn hit(&self, x: usize, y: usize) -> Option<usize> {
        self.order.iter().rev().copied().find(|&slot| {
            self.windows[slot].is_some_and(|w| !w.minimized && w.outer().contains(x, y))
        })
    }

    fn route_pointer(&mut self, slot: usize, buttons: u8) {
        let content = self.windows[slot].unwrap().content();
        self.enqueue(
            slot,
            Event::Pointer {
                x: self
                    .pointer_x
                    .saturating_sub(content.x)
                    .min(protocol::WIDTH - 1) as u32,
                y: self
                    .pointer_y
                    .saturating_sub(content.y)
                    .min(protocol::HEIGHT - 1) as u32,
                buttons,
            },
        );
    }

    fn process(&mut self, event: HardwareEvent) -> bool {
        match event {
            // Loss is per device (spec 0017a-D3). A pointer loss cancels the
            // shell's held interaction and resets each app's pointer state;
            // events already queued for apps, keys included, keep their
            // order. A keyboard loss sends apps nothing: every key record
            // carries its full modifier snapshot, and app Reset and Focus
            // clear only pointer and focus state.
            HardwareEvent::Lost { pointer, .. } => {
                if pointer {
                    self.capture = Capture::None;
                    self.buttons = 0;
                    self.clicks_armed = false;
                    for slot in 0..protocol::MAX_APPS {
                        self.enqueue(slot, Event::Reset);
                    }
                }
            }
            HardwareEvent::Key { flags, kind, ascii } => {
                if flags & 1 != 0 && flags & (2 | 8 | 16) == 0 {
                    if kind == 5 {
                        return true;
                    }
                    if kind == 4 {
                        let next = match self.focused {
                            Some(current) => (1..=protocol::MAX_APPS)
                                .map(|offset| (current + offset) % protocol::MAX_APPS)
                                .find(|&slot| self.windows[slot].is_some_and(|w| !w.minimized)),
                            None => self.top_visible(),
                        };
                        self.focus(next);
                        return false;
                    }
                }
                // Desktop shortcuts never leak their release edges to apps.
                if kind != 4
                    && kind != 5
                    && let Some(slot) = self.focused
                {
                    self.enqueue(slot, Event::Key { flags, kind, ascii });
                }
            }
            HardwareEvent::Mouse { dx, dy, buttons } => {
                let old = (self.pointer_x, self.pointer_y, self.buttons);
                self.pointer_x = self
                    .pointer_x
                    .saturating_add_signed(dx as isize)
                    .min(self.width - 1);
                self.pointer_y = self
                    .pointer_y
                    .saturating_add_signed(dy as isize)
                    .min(self.height - 1);
                self.pointer_changed |= old != (self.pointer_x, self.pointer_y, buttons);
                let pressed = self.clicks_armed && self.buttons & 1 == 0 && buttons & 1 != 0;
                if buttons == 0 {
                    self.clicks_armed = true;
                }
                self.buttons = buttons;
                // Loss resets app button state too. Do not turn a still-held
                // hardware button into a fresh application click after Reset.
                if !self.clicks_armed && buttons != 0 {
                    return false;
                }
                match self.capture {
                    Capture::Drag { slot, x, y } => {
                        if buttons & 1 != 0 {
                            let mut window = self.windows[slot].unwrap();
                            let next_x = self
                                .pointer_x
                                .saturating_sub(x)
                                .min(self.width - OUTER_WIDTH - 8);
                            let next_y = self
                                .pointer_y
                                .saturating_sub(y)
                                .clamp(40, self.height - TASKBAR_HEIGHT - OUTER_HEIGHT - 8);
                            if (next_x, next_y) != (window.x, window.y) {
                                self.invalidate(window.damage());
                                window.x = next_x;
                                window.y = next_y;
                                self.windows[slot] = Some(window);
                                self.invalidate(window.damage());
                                self.log(b"move", &[slot as u64, next_x as u64, next_y as u64]);
                            }
                        } else {
                            self.capture = Capture::None;
                        }
                        return false;
                    }
                    Capture::Client(slot) => {
                        self.route_pointer(slot, buttons);
                        if buttons == 0 {
                            self.capture = Capture::None;
                        }
                        return false;
                    }
                    Capture::None => {}
                }
                if pressed && self.taskbar().contains(self.pointer_x, self.pointer_y) {
                    for kind in [protocol::NOTES, protocol::COUNTER] {
                        if self.launcher(kind).contains(self.pointer_x, self.pointer_y) {
                            self.launch(kind);
                            return false;
                        }
                    }
                    for slot in 0..protocol::MAX_APPS {
                        if self.windows[slot].is_some()
                            && self
                                .task_button(slot)
                                .contains(self.pointer_x, self.pointer_y)
                        {
                            self.restore(slot);
                            return false;
                        }
                    }
                }
                // The toast sits above the windows: pointer events inside it
                // reach no window (0029-D8).
                if self.launch_failed.is_some()
                    && self.toast().contains(self.pointer_x, self.pointer_y)
                {
                    return false;
                }
                if let Some(slot) = self.hit(self.pointer_x, self.pointer_y) {
                    let window = self.windows[slot].unwrap();
                    if pressed {
                        self.focus(Some(slot));
                        if window
                            .close_button()
                            .contains(self.pointer_x, self.pointer_y)
                        {
                            self.close(slot);
                        } else if window
                            .minimize_button()
                            .contains(self.pointer_x, self.pointer_y)
                        {
                            self.minimize(slot);
                        } else if self.pointer_y < window.y + 29 {
                            self.capture = Capture::Drag {
                                slot,
                                x: self.pointer_x - window.x,
                                y: self.pointer_y - window.y,
                            };
                        } else if window.stopped()
                            && window
                                .relaunch_button()
                                .contains(self.pointer_x, self.pointer_y)
                        {
                            self.relaunch(slot);
                        } else {
                            self.capture = Capture::Client(slot);
                            // This press followed an observed hardware release.
                            // Establish that baseline after the focus event, so
                            // the app can safely rearm without inventing edges.
                            self.route_pointer(slot, 0);
                            self.route_pointer(slot, buttons);
                        }
                    } else if window.content().contains(self.pointer_x, self.pointer_y) {
                        self.route_pointer(slot, buttons);
                    }
                } else if pressed {
                    self.focus(None);
                }
            }
        }
        false
    }

    fn flush_events(&mut self) {
        for slot in 0..protocol::MAX_APPS {
            let Some(window) = &mut self.windows[slot] else {
                continue;
            };
            for _ in 0..4 {
                if window.pending.length == 0 || window.state != protocol::RUNNING {
                    break;
                }
                let bytes = window.pending.events[0].unwrap().encode().unwrap();
                match syscall(
                    protocol::SYS_APP_EVENT,
                    window.handle,
                    bytes.as_ptr() as u64,
                    bytes.len() as u64,
                ) {
                    0 => window.pending.pop(),
                    -11 => break,
                    _ => {
                        window.pending = Pending::new();
                        break;
                    }
                }
            }
        }
    }

    fn refresh(&mut self, renderer: &mut Renderer) {
        for slot in 0..protocol::MAX_APPS {
            let Some(mut window) = self.windows[slot] else {
                continue;
            };
            let Some(info) = app_info(window.handle) else {
                continue;
            };
            if info.state != window.state {
                window.state = info.state;
                window.pending = Pending::new();
                window.ready = false;
                self.invalidate(window.damage());
                self.invalidate(self.taskbar());
            }
            if info.state == protocol::RUNNING
                && !window.minimized
                && info.revision != 0
                && (!window.ready || info.revision != window.revision)
            {
                window.ready = false;
                if read_texture(renderer, slot, window.handle, info.revision)
                    && app_info(window.handle).is_some_and(|after| {
                        after.state == protocol::RUNNING && after.revision == info.revision
                    })
                {
                    window.revision = info.revision;
                    window.ready = true;
                }
                renderer.damage(window.content());
            }
            self.windows[slot] = Some(window);
        }
    }

    fn paint(&self, renderer: &mut Renderer) {
        // Color only for meaning (0029-D4): the decorative right-half panel,
        // the accent rule under the headline and the window shadows are gone.
        renderer.fill(Rect::new(0, 0, self.width, self.height), render::BASE);
        renderer.fill(Rect::new(0, 0, self.width, 36), render::SURFACE);
        renderer.fill(Rect::new(0, 35, self.width, 1), render::BORDER_SUBTLE);
        renderer.text(20, 10, b"MIRROR", WORDMARK.scale(), render::TEXT_PRIMARY);
        renderer.text(
            120,
            14,
            b"Native desktop",
            Role::Caption.scale(),
            render::TEXT_SECONDARY,
        );
        renderer.text(
            self.width - 146,
            14,
            b"Esc returns to console",
            Role::Caption.scale(),
            render::TEXT_SECONDARY,
        );
        renderer.text(
            48,
            84,
            b"Your workspace",
            Role::Display.scale(),
            render::TEXT_PRIMARY,
        );
        renderer.text(
            48,
            118,
            b"Open an app below to get started.",
            Role::Body.scale(),
            render::TEXT_SECONDARY,
        );
        for &slot in &self.order {
            let Some(window) = self.windows[slot] else {
                continue;
            };
            if window.minimized {
                continue;
            }
            // Today's chrome geometry and colors, mapped onto the tokens; the
            // design's 38 px titlebar and caption buttons are 0029-D9's.
            let focused = self.focused == Some(slot);
            let outer = window.outer();
            renderer.fill(outer, render::SURFACE);
            // 0029-D9's frame colors land before its geometry: the focused
            // frame must read stronger than the unfocused one.
            renderer.border(
                outer,
                if focused {
                    render::BORDER_STRONG
                } else {
                    render::BORDER_SUBTLE
                },
            );
            renderer.fill(
                Rect::new(window.x + 1, window.y + 1, OUTER_WIDTH - 2, 28),
                if focused {
                    render::OVERLAY
                } else {
                    render::SURFACE
                },
            );
            renderer.fill(
                Rect::new(window.x + 12, window.y + 11, 6, 6),
                render::ACCENT,
            );
            renderer.text(
                window.x + 26,
                window.y + 11,
                title(window.kind),
                Role::Label.scale(),
                render::TEXT_PRIMARY,
            );
            renderer.text(
                window.x + 190,
                window.y + 11,
                if window.stopped() { b"STOPPED" } else { b"APP" },
                Role::Overline.scale(),
                render::TEXT_SECONDARY,
            );
            renderer.fill(window.minimize_button(), render::SURFACE);
            renderer.fill(
                Rect::new(window.x + 272, window.y + 17, 10, 2),
                render::TEXT_SECONDARY,
            );
            renderer.fill(window.close_button(), render::SURFACE);
            renderer.text(
                window.x + 301,
                window.y + 10,
                b"x",
                Role::Label.scale(),
                render::TEXT_SECONDARY,
            );
            renderer.fill(window.content(), render::SURFACE);
            if window.stopped() {
                // Errors are icon and text, and offer recovery (0029-D8).
                renderer.text(
                    window.x + 8,
                    window.y + 74,
                    b"!",
                    STOPPED_TITLE.scale(),
                    render::DANGER,
                );
                renderer.text(
                    window.x + 24,
                    window.y + 74,
                    b"Application stopped",
                    STOPPED_TITLE.scale(),
                    render::DANGER,
                );
                renderer.text(
                    window.x + 24,
                    window.y + 105,
                    b"Relaunch it, or close this window.",
                    Role::Body.scale(),
                    render::TEXT_SECONDARY,
                );
                let button = window.relaunch_button();
                renderer.fill(button, render::ACCENT);
                renderer.text(
                    button.x + 24,
                    button.y + 10,
                    b"Relaunch",
                    Role::Label.scale(),
                    render::ON_ACCENT,
                );
            } else if !window.ready {
                renderer.text(
                    window.x + 24,
                    window.y + 80,
                    b"Opening application...",
                    Role::Body.scale(),
                    render::TEXT_SECONDARY,
                );
            }
        }
        renderer.fill(self.taskbar(), render::SURFACE);
        renderer.fill(
            Rect::new(0, self.height - TASKBAR_HEIGHT, self.width, 1),
            render::BORDER_SUBTLE,
        );
        for kind in [protocol::NOTES, protocol::COUNTER] {
            let button = self.launcher(kind);
            renderer.fill(button, render::OVERLAY);
            renderer.border(button, render::BORDER_STRONG);
            renderer.text(
                button.x + 12,
                button.y + 12,
                title(kind),
                Role::Label.scale(),
                render::TEXT_PRIMARY,
            );
        }
        for slot in 0..protocol::MAX_APPS {
            let Some(window) = self.windows[slot] else {
                continue;
            };
            let button = self.task_button(slot);
            let active = self.focused == Some(slot) && !window.minimized;
            renderer.fill(
                button,
                if active {
                    render::ACCENT_SUBTLE
                } else {
                    render::OVERLAY
                },
            );
            renderer.text(
                button.x + 10,
                button.y + 12,
                title(window.kind),
                Role::Label.scale(),
                if active {
                    render::ACCENT_SUBTLE_TEXT
                } else {
                    render::TEXT_PRIMARY
                },
            );
            renderer.fill(
                Rect::new(button.x + 8, button.y + 28, button.width - 16, 2),
                if window.minimized {
                    render::TEXT_TERTIARY
                } else {
                    render::ACCENT
                },
            );
        }
        if let Some(refusal) = self.launch_failed {
            self.paint_toast(renderer, refusal);
        }
    }

    /// The refused-launch toast (0029-D8), painted after the windows: an
    /// elevated card with a border-strong frame (no shadow here, and
    /// border-subtle is 1.21:1 on elevated), a 3 px bar and `!` in warning or
    /// danger, a text-primary title and a text-secondary body.
    fn paint_toast(&self, renderer: &mut Renderer, refusal: Refusal) {
        let toast = self.toast();
        let (color, heading, body) = match refusal {
            Refusal::NoSlot => (
                render::WARNING,
                &b"Two windows are open"[..],
                &b"Close one to open another."[..],
            ),
            Refusal::LaunchFailed => (
                render::DANGER,
                &b"The app could not start"[..],
                &b"Click its launcher to try again."[..],
            ),
        };
        renderer.fill(toast, render::ELEVATED);
        renderer.border(toast, render::BORDER_STRONG);
        renderer.fill(Rect::new(toast.x, toast.y, 3, toast.height), color);
        renderer.text(toast.x + 15, toast.y + 15, b"!", Role::Label.scale(), color);
        renderer.text(
            toast.x + 27,
            toast.y + 15,
            heading,
            Role::Label.scale(),
            render::TEXT_PRIMARY,
        );
        renderer.text(
            toast.x + 27,
            toast.y + 31,
            body,
            Role::Caption.scale(),
            render::TEXT_SECONDARY,
        );
    }

    fn render(&mut self, renderer: &mut Renderer, surface: u64) -> bool {
        if self.scene_dirty {
            renderer.update_scene(|renderer| self.paint(renderer));
            self.scene_dirty = false;
        }
        if let Some(rect) = self.damaged.take() {
            renderer.damage(rect);
        }
        renderer.move_cursor(self.pointer_x, self.pointer_y);
        let empty = WindowView {
            slot: 0,
            outer: Rect::new(0, 0, 0, 0),
            content: Rect::new(0, 0, 0, 0),
            ready: false,
        };
        // One view per window, then the toast: last and never ready, so its
        // palette pixels stay over any app content beneath it.
        let mut views = [empty; protocol::MAX_APPS + 1];
        let mut count = 0;
        for &slot in &self.order {
            if let Some(window) = self.windows[slot]
                && !window.minimized
            {
                views[count] = WindowView {
                    slot,
                    outer: window.outer(),
                    content: window.content(),
                    ready: window.ready,
                };
                count += 1;
            }
        }
        if self.launch_failed.is_some() {
            views[count] = WindowView {
                outer: self.toast(),
                ..empty
            };
            count += 1;
        }
        renderer.flush_windows(surface, &views[..count])
    }
}

fn title(kind: u64) -> &'static [u8] {
    if kind == protocol::NOTES {
        b"Notes"
    } else {
        b"Counter"
    }
}

fn app_info(handle: u64) -> Option<AppInfo> {
    let mut bytes = [0; protocol::INFO_SIZE];
    if syscall(
        protocol::SYS_APP_INFO,
        handle,
        bytes.as_mut_ptr() as u64,
        bytes.len() as u64,
    ) != bytes.len() as i64
    {
        return None;
    }
    AppInfo::decode(&bytes).ok()
}

fn read_texture(renderer: &mut Renderer, slot: usize, handle: u64, revision: u64) -> bool {
    let mut packet = [0u8; protocol::MAX_READ_PACKET];
    for y in (0..protocol::HEIGHT).step_by(protocol::TILE) {
        for x in (0..protocol::WIDTH).step_by(protocol::TILE) {
            let request = TileRequest {
                revision,
                x: x as u32,
                y: y as u32,
                width: protocol::TILE as u32,
                height: protocol::TILE as u32,
            };
            packet[..protocol::READ_HEADER].copy_from_slice(&request.encode_header().unwrap());
            let length = request.packet_len().unwrap();
            if syscall(
                protocol::SYS_APP_READ,
                handle,
                packet.as_mut_ptr() as u64,
                length as u64,
            ) != length as i64
            {
                return false;
            }
            renderer.texture_tile(
                slot,
                Rect::new(x, y, protocol::TILE, protocol::TILE),
                &packet[protocol::READ_HEADER..length],
            );
        }
    }
    true
}

fn log_values(console: u64, label: &[u8], values: &[u64]) {
    let mut bytes = [0u8; 128];
    let prefix = b"Mirror shell: ";
    bytes[..prefix.len()].copy_from_slice(prefix);
    let mut length = prefix.len();
    bytes[length..length + label.len()].copy_from_slice(label);
    length += label.len();
    for value in values {
        bytes[length] = b' ';
        length += 1;
        let mut number = *value;
        let mut digits = [0u8; 20];
        let mut start = digits.len();
        loop {
            start -= 1;
            digits[start] = b'0' + (number % 10) as u8;
            number /= 10;
            if number == 0 {
                break;
            }
        }
        bytes[length..length + digits.len() - start].copy_from_slice(&digits[start..]);
        length += digits.len() - start;
    }
    bytes[length] = b'\n';
    write(console, &bytes[..length + 1]);
}

pub fn run(smoke: bool) -> ! {
    let console = boot_handle(0);
    let surface = boot_handle(3);
    let input = boot_handle(4);
    let supervisor = boot_handle(5);
    let (width, height) = surface_dimensions(surface);
    if smoke {
        smoke_test(console, supervisor);
    }
    if width < 480 || height < 360 {
        write(console, b"Mirror shell: display is too small\n");
        exit(0);
    }
    let mut renderer = Renderer::new(width, height);
    let mut shell = Shell::new(width, height, supervisor);
    require(shell.render(&mut renderer, surface), 143);
    write(console, b"Mirror shell: ready\n");
    loop {
        for _ in 0..16 {
            match next_input(SYS_INPUT_POLL, input) {
                Ok(event) => {
                    if shell.process(event) {
                        exit(0);
                    }
                }
                Err(-11) => break,
                Err(_) => exit(144),
            }
        }
        shell.flush_events();
        shell.refresh(&mut renderer);
        if shell.render(&mut renderer, surface) {
            write(console, b"Mirror shell: frame\n");
        }
        shell.flush_logs(console);
        require(syscall(SYS_SLEEP, 1, 0, 0) == 0, 145);
    }
}

fn smoke_test(console: u64, supervisor: u64) -> ! {
    policy_checks();
    // The same launch, focus, routing and close path is exercised without a
    // renderer; frame readback below verifies real app-owned published pixels.
    let mut shell = Shell::new(1280, 800, supervisor);
    let first = shell.launch(protocol::NOTES).unwrap_or_else(|| exit(150));
    // Observe the focused frame before requesting its inverse, so a still-
    // queued focus change cannot be mistaken for a completed state transition.
    wait_pixel(&mut shell, first, 12, 60, FOCUSED_BORDER);
    let second = shell.launch(protocol::NOTES).unwrap_or_else(|| exit(151));
    wait_pixel(&mut shell, first, 12, 60, UNFOCUSED_BORDER);
    wait_pixel(&mut shell, second, 12, 60, FOCUSED_BORDER);
    let first_before = app_info(shell.windows[first].unwrap().handle).unwrap();
    let second_before = app_info(shell.windows[second].unwrap().handle).unwrap();
    require(first_before.pid != second_before.pid, 152);
    shell.process(HardwareEvent::Key {
        flags: 1,
        kind: 1,
        ascii: b'a',
    });
    // Lowercase 'a' has ink at this pixel; the empty document does not.
    wait_pixel(&mut shell, second, 17, 74, INK);
    require(
        app_info(shell.windows[second].unwrap().handle)
            .unwrap()
            .revision
            > second_before.revision,
        162,
    );
    require(
        app_info(shell.windows[first].unwrap().handle)
            .unwrap()
            .revision
            == first_before.revision,
        153,
    );
    let old_handle = shell.windows[first].unwrap().handle;
    let old_pid = shell.windows[first].unwrap().pid;
    shell.close(first);
    require(app_info(old_handle).is_none(), 154);
    let first = shell.launch(protocol::COUNTER).unwrap_or_else(|| exit(155));
    require(
        shell.windows[first].unwrap().handle != old_handle
            && shell.windows[first].unwrap().pid != old_pid,
        156,
    );
    wait_pixel(&mut shell, second, 12, 60, UNFOCUSED_BORDER);
    wait_pixel(&mut shell, first, 12, 60, FOCUSED_BORDER);
    shell.enqueue(first, Event::TestFault);
    let start = syscall(SYS_TICKS, 0, 0, 0);
    let stopped = loop {
        shell.flush_events();
        let info = app_info(shell.windows[first].unwrap().handle).unwrap();
        if info.state == protocol::FAULTED {
            break info;
        }
        require(syscall(SYS_TICKS, 0, 0, 0) - start < 500, 157);
        syscall(SYS_SLEEP, 1, 0, 0);
    };
    shell.focus(Some(second));
    wait_pixel(&mut shell, second, 12, 60, FOCUSED_BORDER);
    let before = app_info(shell.windows[second].unwrap().handle)
        .unwrap()
        .revision;
    shell.process(HardwareEvent::Key {
        flags: 1,
        kind: 1,
        ascii: b'b',
    });
    wait_pixel(&mut shell, second, 23, 74, INK);
    require(
        app_info(shell.windows[second].unwrap().handle)
            .unwrap()
            .revision
            > before,
        163,
    );
    require(
        frame_hash(shell.windows[second].unwrap().handle).is_some(),
        158,
    );
    // Smoke has no renderer, so record the polled stop as refresh() does; the
    // Relaunch button exists only for a stopped window (0029-D8). Then press
    // it at its center through the same Mouse arm as a real click.
    let mut window = shell.windows[first].unwrap();
    let (stopped_handle, stopped_pid) = (window.handle, window.pid);
    window.state = stopped.state;
    window.pending = Pending::new();
    window.ready = false;
    shell.windows[first] = Some(window);
    let button = window.relaunch_button();
    shell.pointer_x = button.x + button.width / 2;
    shell.pointer_y = button.y + button.height / 2;
    for buttons in [0, 1] {
        shell.process(HardwareEvent::Mouse {
            dx: 0,
            dy: 0,
            buttons,
        });
    }
    let relaunched = shell.windows[first].unwrap_or_else(|| exit(171));
    require(
        relaunched.handle != stopped_handle
            && relaunched.pid != stopped_pid
            && relaunched.kind == protocol::COUNTER
            && relaunched.state == protocol::RUNNING
            && app_info(stopped_handle).is_none()
            && app_info(relaunched.handle).is_some_and(|info| info.state == protocol::RUNNING),
        172,
    );
    wait_pixel(&mut shell, first, 12, 60, FOCUSED_BORDER);
    shell.close(first);
    let busy = shell.launch(protocol::COUNTER).unwrap_or_else(|| exit(159));
    wait_pixel(&mut shell, second, 12, 60, UNFOCUSED_BORDER);
    wait_pixel(&mut shell, busy, 12, 60, FOCUSED_BORDER);
    shell.enqueue(busy, Event::TestBusy);
    // This color is published only by the gated diagnostic handler immediately
    // before its CPU-bound loop. A timer/focus frame cannot satisfy this fence.
    wait_pixel(&mut shell, busy, 0, 0, [255, 128, 0, 0]);
    shell.focus(Some(second));
    wait_pixel(&mut shell, second, 12, 60, FOCUSED_BORDER);
    let before = app_info(shell.windows[second].unwrap().handle)
        .unwrap()
        .revision;
    shell.process(HardwareEvent::Key {
        flags: 1,
        kind: 1,
        ascii: b'c',
    });
    wait_pixel(&mut shell, second, 29, 74, INK);
    require(
        app_info(shell.windows[second].unwrap().handle)
            .unwrap()
            .revision
            > before,
        164,
    );
    require(
        read_pixel(shell.windows[busy].unwrap().handle, 0, 0) == Some([255, 128, 0, 0]),
        165,
    );
    // Record the observed publication before closing the busy peer. The app's
    // own text log follows its frame commit and can be delayed by preemption,
    // even after this supervisor has already observed the complete edit.
    log_values(
        console,
        b"busy peer progress",
        &[
            shell.windows[second].unwrap().pid,
            shell.windows[busy].unwrap().pid,
        ],
    );
    shell.close(busy);
    shell.close(second);
    shell.flush_logs(console);
    write(console, b"Mirror shell: checks passed\n");
    exit(0)
}

fn policy_checks() {
    // A focus-loss record arriving at a full queue must establish the new
    // unfocused baseline, even when the caller still reports the old focus.
    let mut queue = Pending::new();
    for _ in 0..PENDING_EVENTS {
        queue.push(
            Event::Key {
                flags: 1,
                kind: 1,
                ascii: b'a',
            },
            true,
        );
    }
    queue.push(Event::Focus(false), true);
    require(
        queue.length == 2
            && queue.events[0] == Some(Event::Reset)
            && queue.events[1] == Some(Event::Focus(false)),
        166,
    );
    // Motion can coalesce, but a button edge and following key retain order.
    queue = Pending::new();
    queue.push(
        Event::Pointer {
            x: 1,
            y: 1,
            buttons: 0,
        },
        true,
    );
    queue.push(
        Event::Pointer {
            x: 2,
            y: 2,
            buttons: 0,
        },
        true,
    );
    queue.push(
        Event::Pointer {
            x: 2,
            y: 2,
            buttons: 1,
        },
        true,
    );
    queue.push(
        Event::Key {
            flags: 1,
            kind: 1,
            ascii: b'b',
        },
        true,
    );
    require(
        queue.length == 3
            && queue.events[0]
                == Some(Event::Pointer {
                    x: 2,
                    y: 2,
                    buttons: 0,
                })
            && queue.events[1]
                == Some(Event::Pointer {
                    x: 2,
                    y: 2,
                    buttons: 1,
                })
            && queue.events[2]
                == Some(Event::Key {
                    flags: 1,
                    kind: 1,
                    ascii: b'b',
                }),
        167,
    );
    let mut shell = Shell::new(1280, 800, 0);
    shell.windows[0] = Some(Window {
        handle: 0,
        pid: 0,
        kind: protocol::NOTES,
        x: 80,
        y: 100,
        minimized: false,
        state: protocol::RUNNING,
        revision: 0,
        ready: false,
        pending: Pending::new(),
    });
    shell.focused = Some(0);
    shell.pointer_x = 100;
    shell.pointer_y = 200;
    shell.clicks_armed = true;
    shell.enqueue(
        0,
        Event::Key {
            flags: 1,
            kind: 1,
            ascii: b'x',
        },
    );
    // A pointer-only loss keeps the key already queued for the focused app
    // and follows it with a pointer Reset (spec 0017a, acceptance test 4).
    shell.process(HardwareEvent::Lost {
        keyboard: false,
        pointer: true,
    });
    let pending = shell.windows[0].unwrap().pending;
    require(
        pending.length == 2
            && pending.events[0]
                == Some(Event::Key {
                    flags: 1,
                    kind: 1,
                    ascii: b'x',
                })
            && pending.events[1] == Some(Event::Reset),
        168,
    );
    shell.process(HardwareEvent::Mouse {
        dx: 1,
        dy: 0,
        buttons: 1,
    });
    require(
        shell.pointer_x == 101
            && shell.buttons == 1
            && !shell.clicks_armed
            && matches!(shell.capture, Capture::None)
            && shell.windows[0].unwrap().pending.length == 2,
        169,
    );
    shell.process(HardwareEvent::Mouse {
        dx: 0,
        dy: 0,
        buttons: 0,
    });
    shell.process(HardwareEvent::Mouse {
        dx: 0,
        dy: 0,
        buttons: 1,
    });
    let pending = shell.windows[0].unwrap().pending;
    require(
        shell.clicks_armed
            && matches!(shell.capture, Capture::Client(0))
            && pending.length == 4
            && pending.events[2]
                == Some(Event::Pointer {
                    x: 20,
                    y: 71,
                    buttons: 0,
                })
            && pending.events[3]
                == Some(Event::Pointer {
                    x: 20,
                    y: 71,
                    buttons: 1,
                }),
        170,
    );
    // A keyboard-only loss sends apps nothing and keeps the pointer's held
    // client capture, buttons and click arming.
    shell.process(HardwareEvent::Lost {
        keyboard: true,
        pointer: false,
    });
    require(
        shell.windows[0].unwrap().pending.length == 4
            && matches!(shell.capture, Capture::Client(0))
            && shell.buttons == 1
            && shell.clicks_armed,
        173,
    );
}

fn wait_pixel(shell: &mut Shell, slot: usize, x: u32, y: u32, expected: [u8; 4]) {
    let start = syscall(SYS_TICKS, 0, 0, 0);
    loop {
        shell.flush_events();
        if read_pixel(shell.windows[slot].unwrap().handle, x, y) == Some(expected) {
            break;
        }
        require(syscall(SYS_TICKS, 0, 0, 0) - start < 500, 161);
        syscall(SYS_SLEEP, 1, 0, 0);
    }
}

fn read_pixel(handle: u64, x: u32, y: u32) -> Option<[u8; 4]> {
    let info = app_info(handle)?;
    if info.revision == 0 {
        return None;
    }
    let request = TileRequest {
        revision: info.revision,
        x,
        y,
        width: 1,
        height: 1,
    };
    let mut packet = [0u8; protocol::READ_HEADER + 4];
    packet[..protocol::READ_HEADER].copy_from_slice(&request.encode_header().ok()?);
    if syscall(
        protocol::SYS_APP_READ,
        handle,
        packet.as_mut_ptr() as u64,
        packet.len() as u64,
    ) != packet.len() as i64
    {
        return None;
    }
    if app_info(handle)?.revision != info.revision {
        return None;
    }
    Some(packet[protocol::READ_HEADER..].try_into().unwrap())
}

fn frame_hash(handle: u64) -> Option<u64> {
    let revision = app_info(handle)?.revision;
    let mut packet = [0u8; protocol::MAX_READ_PACKET];
    let mut hash = 0xcbf2_9ce4_8422_2325u64;
    for y in (0..protocol::HEIGHT).step_by(protocol::TILE) {
        for x in (0..protocol::WIDTH).step_by(protocol::TILE) {
            let request = TileRequest {
                revision,
                x: x as u32,
                y: y as u32,
                width: protocol::TILE as u32,
                height: protocol::TILE as u32,
            };
            packet[..protocol::READ_HEADER].copy_from_slice(&request.encode_header().ok()?);
            let length = request.packet_len().ok()?;
            if syscall(
                protocol::SYS_APP_READ,
                handle,
                packet.as_mut_ptr() as u64,
                length as u64,
            ) != length as i64
            {
                return None;
            }
            for &byte in &packet[protocol::READ_HEADER..length] {
                hash = (hash ^ u64::from(byte)).wrapping_mul(0x100_0000_01b3);
            }
        }
    }
    (app_info(handle)?.revision == revision).then_some(hash)
}
