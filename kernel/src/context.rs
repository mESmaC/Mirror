//! Saved CPU state belonging to one execution context. Process resources such
//! as mappings and handles remain in their process owner. Each scheduled thread
//! owns one context, including its independent TLS and floating-point state.

use crate::{fpu, interrupts};

pub(crate) struct Context {
    pub frame: interrupts::TrapFrame,
    pub extended: fpu::State,
}

impl Context {
    pub(crate) fn new(entry: u64, stack: u64, argument: u64) -> Self {
        Self {
            frame: interrupts::user_frame(entry, stack, argument),
            extended: fpu::State::new(),
        }
    }
}
