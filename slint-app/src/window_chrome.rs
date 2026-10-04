//! Pointer state for Slint chrome surrounding a native child window.
//! Positions are physical desktop pixels, so dragging never mixes DPI units.
use raw_window_handle::{HasWindowHandle, RawWindowHandle};
use slint::{ComponentHandle, PhysicalPosition};
use windows_sys::Win32::{
    Foundation::POINT,
    UI::WindowsAndMessaging::{GetCursorPos, IsChild, WindowFromPoint},
};

pub fn cursor_position() -> Option<PhysicalPosition> {
    let mut point = POINT { x: 0, y: 0 };
    (unsafe { GetCursorPos(&mut point) } != 0).then_some(PhysicalPosition::new(point.x, point.y))
}

pub fn update_hover(window: &crate::NoteWindow) {
    let slint_handle = window.window().window_handle();
    let Ok(handle) = slint_handle.window_handle() else {
        return;
    };
    let RawWindowHandle::Win32(handle) = handle.as_raw() else {
        return;
    };
    let parent = handle.hwnd.get() as windows_sys::Win32::Foundation::HWND;
    let hovered = cursor_position().is_some_and(|position| {
        let hit = unsafe {
            WindowFromPoint(POINT {
                x: position.x,
                y: position.y,
            })
        };
        hit == parent || unsafe { IsChild(parent, hit) } != 0
    });
    if window.get_pointer_inside() != hovered {
        window.set_pointer_inside(hovered);
    }
}

pub fn dragged_position(
    start_window: PhysicalPosition,
    start_cursor: PhysicalPosition,
    cursor: PhysicalPosition,
) -> PhysicalPosition {
    PhysicalPosition::new(
        start_window
            .x
            .saturating_add(cursor.x.saturating_sub(start_cursor.x)),
        start_window
            .y
            .saturating_add(cursor.y.saturating_sub(start_cursor.y)),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dragging_uses_screen_delta_even_across_negative_monitor_coordinates() {
        assert_eq!(
            dragged_position(
                PhysicalPosition::new(-800, 200),
                PhysicalPosition::new(-600, 210),
                PhysicalPosition::new(-650, 280)
            ),
            PhysicalPosition::new(-850, 270)
        );
    }
}
