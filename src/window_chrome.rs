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

pub fn drag_moved(start: PhysicalPosition, cursor: PhysicalPosition, scale: f32) -> bool {
    let threshold = (8.0 * scale.max(1.0)).ceil() as u32;
    start.x.abs_diff(cursor.x) >= threshold || start.y.abs_diff(cursor.y) >= threshold
}

pub fn contains(bounds: (PhysicalPosition, slint::PhysicalSize), cursor: PhysicalPosition) -> bool {
    let (position, size) = bounds;
    let x = i64::from(cursor.x) - i64::from(position.x);
    let y = i64::from(cursor.y) - i64::from(position.y);
    x >= 0 && y >= 0 && x < i64::from(size.width) && y < i64::from(size.height)
}

/// A restored window must keep its title bar grabbable: the middle of its top
/// edge has to lie on a connected monitor, or the saved position is dropped.
pub fn title_on_monitor(
    position: PhysicalPosition,
    width: u32,
    monitors: &[(PhysicalPosition, slint::PhysicalSize)],
) -> bool {
    let grab = PhysicalPosition::new(
        position.x.saturating_add((width / 2) as i32),
        position.y.saturating_add(16),
    );
    monitors.iter().any(|&monitor| contains(monitor, grab))
}

#[derive(Clone, Copy)]
pub enum DropDestination {
    IntoBoard,
    OutOfBoard,
}

pub fn drop_allowed(
    moved: bool,
    eligible: bool,
    cancelled: bool,
    bounds: Option<(PhysicalPosition, slint::PhysicalSize)>,
    cursor: PhysicalPosition,
    destination: DropDestination,
) -> bool {
    moved
        && eligible
        && !cancelled
        && bounds.is_some_and(|bounds| {
            contains(bounds, cursor) == matches!(destination, DropDestination::IntoBoard)
        })
}

pub fn client_bounds(window: &slint::Window) -> Option<(PhysicalPosition, slint::PhysicalSize)> {
    use i_slint_backend_winit::WinitWindowAccessor;
    window
        .with_winit_window(|host| {
            let position = host.inner_position().ok()?;
            let size = host.inner_size();
            Some((
                PhysicalPosition::new(position.x, position.y),
                slint::PhysicalSize::new(size.width, size.height),
            ))
        })
        .flatten()
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn cancelled_clicks_hidden_boards_and_missing_bounds_never_complete_drops() {
        let bounds = Some((
            PhysicalPosition::new(-500, 100),
            slint::PhysicalSize::new(400, 600),
        ));
        let inside = PhysicalPosition::new(-200, 200);
        let outside = PhysicalPosition::new(20, 200);
        for (point, destination) in [
            (inside, DropDestination::IntoBoard),
            (outside, DropDestination::OutOfBoard),
        ] {
            assert!(drop_allowed(true, true, false, bounds, point, destination));
            assert!(!drop_allowed(
                false,
                true,
                false,
                bounds,
                point,
                destination
            ));
            assert!(!drop_allowed(
                true,
                false,
                false,
                bounds,
                point,
                destination
            ));
            assert!(!drop_allowed(true, true, true, bounds, point, destination));
            assert!(!drop_allowed(true, true, false, None, point, destination));
        }
        assert!(!drop_allowed(
            true,
            true,
            false,
            bounds,
            outside,
            DropDestination::IntoBoard
        ));
        assert!(!drop_allowed(
            true,
            true,
            false,
            bounds,
            inside,
            DropDestination::OutOfBoard
        ));
    }
    #[test]
    fn restored_title_bar_must_land_on_a_connected_monitor() {
        let monitors = [
            (
                PhysicalPosition::new(-2560, 0),
                slint::PhysicalSize::new(2560, 1440),
            ),
            (
                PhysicalPosition::new(0, 0),
                slint::PhysicalSize::new(2560, 1440),
            ),
        ];
        assert!(title_on_monitor(
            PhysicalPosition::new(-1500, 100),
            416,
            &monitors
        ));
        assert!(title_on_monitor(
            PhysicalPosition::new(2300, 100),
            416,
            &monitors
        ));
        // Saved on a monitor that is now unplugged, or dragged above the top.
        assert!(!title_on_monitor(
            PhysicalPosition::new(5200, 100),
            416,
            &monitors
        ));
        assert!(!title_on_monitor(
            PhysicalPosition::new(100, -60),
            416,
            &monitors
        ));
        assert!(!title_on_monitor(PhysicalPosition::new(100, 100), 416, &[]));
    }
    #[test]
    fn drop_bounds_and_drag_threshold_use_physical_negative_desktop_coordinates() {
        let bounds = (
            PhysicalPosition::new(-500, 100),
            slint::PhysicalSize::new(400, 600),
        );
        assert!(contains(bounds, PhysicalPosition::new(-500, 100)));
        assert!(contains(bounds, PhysicalPosition::new(-101, 699)));
        for point in [
            PhysicalPosition::new(-501, 100),
            PhysicalPosition::new(-100, 100),
            PhysicalPosition::new(-500, 700),
        ] {
            assert!(!contains(bounds, point));
        }
        let start = PhysicalPosition::new(-200, 100);
        assert!(!drag_moved(start, PhysicalPosition::new(-193, 100), 1.0));
        assert!(drag_moved(start, PhysicalPosition::new(-192, 100), 1.0));
        assert!(!drag_moved(start, PhysicalPosition::new(-185, 100), 2.0));
        assert!(drag_moved(start, PhysicalPosition::new(-184, 100), 2.0));
    }
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
