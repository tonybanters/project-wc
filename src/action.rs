use crate::state::State;
use smithay::{
    desktop::Window, reexports::wayland_protocols::xdg::shell::server::xdg_toplevel,
    utils::SERIAL_COUNTER,
};

pub enum Action {
    FocusNext,
    FocusPrevious,
}

enum Direction {
    Next,
    Previous,
}

impl Action {
    pub fn execute(self, state: &mut State) {
        match self {
            Action::FocusNext => {
                change_focus(Direction::Next, state);
            }
            Action::FocusPrevious => {
                change_focus(Direction::Previous, state);
            }
        };
    }
}

fn change_focus(direction: Direction, state: &mut State) {
    let keyboard = state.projectwc.seat.get_keyboard().unwrap();
    let serial = SERIAL_COUNTER.next_serial();

    let windows: Vec<Window> = state.projectwc.space.elements().cloned().collect();
    if windows.is_empty() {
        return;
    }

    let current_focus = keyboard.current_focus();
    let current_idx = current_focus.and_then(|surf| {
        state
            .projectwc
            .window_for_surface(&surf)
            .and_then(|w| windows.iter().position(|win| win == &w))
    });

    let target_idx = match (direction, current_idx) {
        (Direction::Next, Some(i)) => usize::min(i + 1, windows.len() - 1),
        (Direction::Previous, Some(i)) => i.saturating_sub(1),
        _ => return,
    };

    if let Some(prev_idx) = current_idx {
        if let Some(prev_toplevel) = windows[prev_idx].toplevel() {
            prev_toplevel.with_pending_state(|state| {
                state.states.unset(xdg_toplevel::State::Activated);
            });
            prev_toplevel.send_pending_configure();
        }
    }

    let target = &windows[target_idx];

    if let Some(toplevel) = target.toplevel() {
        toplevel.with_pending_state(|state| {
            state.states.set(xdg_toplevel::State::Activated);
        });
        toplevel.send_pending_configure();
        keyboard.set_focus(state, Some(toplevel.wl_surface().clone()), serial);
    }
}
