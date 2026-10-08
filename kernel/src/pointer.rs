//! Main-context pointer state, independent of framebuffer ownership or IRQs.
//!
//! Motion is clamped to the current surface. Clicks are left-button edges, not
//! held-button repeats; initialization and input loss require a release before
//! accepting clicks so a missing earlier packet cannot create a phantom press.

use crate::mouse::{LEFT, MIDDLE, MouseEvent, RIGHT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Update {
    pub moved: bool,
    pub left_pressed: bool,
}

pub struct Pointer {
    x: usize,
    y: usize,
    max_x: usize,
    max_y: usize,
    buttons: u8,
    clicks_armed: bool,
}

impl Pointer {
    pub fn new(width: usize, height: usize) -> Self {
        let max_x = width.saturating_sub(1);
        let max_y = height.saturating_sub(1);
        Self {
            x: (width / 2).min(max_x),
            y: (height / 2).min(max_y),
            max_x,
            max_y,
            buttons: 0,
            clicks_armed: false,
        }
    }

    pub fn position(&self) -> (usize, usize) {
        (self.x, self.y)
    }

    pub fn apply(&mut self, event: MouseEvent) -> Update {
        let previous = self.position();
        self.x = self
            .x
            .saturating_add_signed(isize::from(event.dx))
            .min(self.max_x);
        self.y = self
            .y
            .saturating_add_signed(isize::from(event.dy))
            .min(self.max_y);
        let buttons = event.buttons & (LEFT | RIGHT | MIDDLE);
        let left_pressed = self.clicks_armed && self.buttons & LEFT == 0 && buttons & LEFT != 0;
        if buttons == 0 {
            self.clicks_armed = true;
        }
        self.buttons = buttons;
        Update {
            moved: previous != self.position(),
            left_pressed,
        }
    }

    pub fn input_lost(&mut self) {
        self.buttons = 0;
        self.clicks_armed = false;
    }
}

#[cfg(feature = "boot-smoke")]
pub fn self_test() {
    fn event(dx: i16, dy: i16, buttons: u8) -> MouseEvent {
        MouseEvent { dx, dy, buttons }
    }
    let mut pointer = Pointer::new(1280, 800);
    assert_eq!(pointer.position(), (640, 400));
    assert_eq!(
        pointer.apply(event(0, 0, LEFT)),
        Update {
            moved: false,
            left_pressed: false
        }
    );
    assert_eq!(
        pointer.apply(event(1, -1, LEFT)),
        Update {
            moved: true,
            left_pressed: false
        }
    );
    assert_eq!(pointer.position(), (641, 399));
    assert!(!pointer.apply(event(0, 0, 0)).left_pressed);
    assert!(pointer.apply(event(0, 0, LEFT)).left_pressed);
    assert!(!pointer.apply(event(0, 0, LEFT)).left_pressed);
    assert!(!pointer.apply(event(0, 0, RIGHT)).left_pressed);
    assert!(pointer.apply(event(0, 0, LEFT | RIGHT)).left_pressed);
    assert!(
        !pointer
            .apply(event(0, 0, LEFT | RIGHT | MIDDLE))
            .left_pressed
    );

    pointer.input_lost();
    assert!(!pointer.apply(event(0, 0, LEFT)).left_pressed);
    assert!(!pointer.apply(event(0, 0, RIGHT)).left_pressed);
    assert!(!pointer.apply(event(0, 0, LEFT)).left_pressed);
    assert!(!pointer.apply(event(0, 0, 0)).left_pressed);
    assert!(pointer.apply(event(0, 0, LEFT)).left_pressed);
    assert!(pointer.apply(event(i16::MAX, i16::MAX, 0)).moved);
    assert_eq!(pointer.position(), (1279, 799));
    assert!(!pointer.apply(event(i16::MAX, i16::MAX, 0)).moved);
    assert!(pointer.apply(event(i16::MIN, i16::MIN, 0)).moved);
    assert_eq!(pointer.position(), (0, 0));
    assert!(!pointer.apply(event(i16::MIN, i16::MIN, 0)).moved);

    for dimensions in [(0, 0), (1, 1)] {
        let mut tiny = Pointer::new(dimensions.0, dimensions.1);
        assert_eq!(tiny.position(), (0, 0));
        assert!(!tiny.apply(event(i16::MAX, i16::MIN, 0)).moved);
        assert_eq!(tiny.position(), (0, 0));
    }
    let mut enormous = Pointer::new(usize::MAX, usize::MAX);
    enormous.x = usize::MAX - 2;
    enormous.y = 1;
    enormous.apply(event(i16::MAX, i16::MIN, 0));
    assert_eq!(enormous.position(), (usize::MAX - 1, 0));
    crate::serial::println!("MIRROR_POINTER_TESTS_OK");
}
