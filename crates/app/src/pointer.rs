//! Where a pointer event goes, given what covers the window: the modal
//! overlays (palette, menu, picker, confirmation) and the toast. Pure
//! functions, so they test without a window.

use ghostrealm_core::{PaneId, Rect, SurfaceId, VtabId};

use crate::app_state::PICKER_SID;
use crate::plugin::rect_contains;

/// What a clickable chrome button does. All buttons share hover-highlight and
/// arm-on-press/fire-on-release behaviour (see the window's
/// `arm_button`/`fire_button`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ButtonAction {
    /// Open a new workspace (the sidebar's "+ New workspace").
    NewVtab,
    /// Close a workspace (vtab) from the sidebar.
    CloseVtab(VtabId),
    /// Close one tab (surface) in a pane's horizontal tab strip.
    CloseSurface(VtabId, PaneId, SurfaceId),
    /// Show/hide the workspace sidebar.
    ToggleSidebar,
    /// Open a plugin's content in the given pane (the "nothing open" picker).
    OpenPlugin(VtabId, PaneId, &'static str),
    /// A button a view registered while painting: (owning surface, view-local id).
    View(SurfaceId, u32),
    /// Confirm / cancel the floating picker.
    PickerConfirm,
    PickerCancel,
    /// Accept / cancel the open confirmation.
    ConfirmAccept,
    ConfirmCancel,
    ConfirmDiscard,
}

impl ButtonAction {
    /// One of the picker's own buttons (its confirm and cancel, and its
    /// browser's).
    fn in_picker(self) -> bool {
        matches!(self, ButtonAction::PickerConfirm | ButtonAction::PickerCancel | ButtonAction::View(PICKER_SID, _))
    }

    /// One of the confirmation's buttons.
    fn in_confirm(self) -> bool {
        matches!(self, ButtonAction::ConfirmAccept | ButtonAction::ConfirmCancel | ButtonAction::ConfirmDiscard)
    }
}

/// What covers the window this frame.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Gate {
    /// The palette or a menu is open: it owns the pointer.
    pub modal: bool,
    /// The floating picker is open: only its buttons are live.
    pub picker: bool,
    /// A confirmation is open: only its buttons are live.
    pub confirm: bool,
    /// The toast's rect, as last drawn.
    pub toast: Option<Rect>,
}

/// Whether the toast covers `pos`: nothing under it takes the pointer.
pub(crate) fn covered(gate: &Gate, (x, y): (f32, f32)) -> bool {
    gate.toast.is_some_and(|r| rect_contains(r, x, y))
}

/// The button at `pos`, if any. The palette and menus own the pointer; while
/// a confirmation or the picker is open only its buttons are live. The
/// sidebar toggle wins any overlap (it can float over a pane's tab strip when
/// the sidebar is hidden); otherwise the first button in paint order takes
/// the hit.
pub(crate) fn button_at(gate: &Gate, buttons: &[(Rect, ButtonAction)], (x, y): (f32, f32)) -> Option<ButtonAction> {
    if gate.modal {
        return None;
    }
    let mut hit = None;
    for &(r, action) in buttons {
        if (gate.confirm && !action.in_confirm()) || (gate.picker && !gate.confirm && !action.in_picker()) {
            continue;
        }
        if rect_contains(r, x, y) {
            if action == ButtonAction::ToggleSidebar {
                return Some(action);
            }
            hit.get_or_insert(action);
        }
    }
    hit
}

/// The index in `buttons` of the one to light up under `pos`.
pub(crate) fn hover_index(_gate: &Gate, buttons: &[(Rect, ButtonAction)], (x, y): (f32, f32)) -> Option<usize> {
    buttons.iter().position(|(r, _)| rect_contains(*r, x, y))
}

/// The view the wheel scrolls: the one `under` the pointer, else the
/// `focused` one; none under the toast.
pub(crate) fn wheel_target<T>(gate: &Gate, pos: (f32, f32), under: Option<T>, focused: Option<T>) -> Option<T> {
    if covered(gate, pos) {
        return None;
    }
    under.or(focused)
}

#[cfg(test)]
mod tests {
    use super::*;

    const R: Rect = Rect { x: 0.0, y: 0.0, w: 10.0, h: 10.0 };
    const IN: (f32, f32) = (5.0, 5.0);

    #[test]
    fn the_first_button_in_paint_order_takes_the_hit() {
        let buttons = [(R, ButtonAction::NewVtab), (R, ButtonAction::PickerCancel)];
        assert_eq!(button_at(&Gate::default(), &buttons, IN), Some(ButtonAction::NewVtab));
        assert_eq!(button_at(&Gate::default(), &buttons, (20.0, 5.0)), None);
        assert_eq!(hover_index(&Gate::default(), &buttons, IN), Some(0));
    }

    #[test]
    fn the_sidebar_toggle_wins_an_overlap() {
        let buttons = [(R, ButtonAction::NewVtab), (R, ButtonAction::ToggleSidebar)];
        assert_eq!(button_at(&Gate::default(), &buttons, IN), Some(ButtonAction::ToggleSidebar));
    }

    #[test]
    fn modals_limit_the_live_buttons() {
        let buttons = [
            (R, ButtonAction::NewVtab),
            (R, ButtonAction::PickerConfirm),
            (R, ButtonAction::ConfirmCancel),
        ];
        let modal = Gate { modal: true, ..Gate::default() };
        assert_eq!(button_at(&modal, &buttons, IN), None);
        let picker = Gate { picker: true, ..Gate::default() };
        assert_eq!(button_at(&picker, &buttons, IN), Some(ButtonAction::PickerConfirm));
        let confirm = Gate { picker: true, confirm: true, ..Gate::default() };
        assert_eq!(button_at(&confirm, &buttons, IN), Some(ButtonAction::ConfirmCancel));
    }

    #[test]
    fn the_toast_covers_only_its_rect() {
        let gate = Gate { toast: Some(R), ..Gate::default() };
        assert!(covered(&gate, IN));
        assert!(!covered(&gate, (20.0, 5.0)));
        assert!(!covered(&Gate::default(), IN));
    }

    #[test]
    fn the_wheel_goes_under_the_pointer_else_to_focus_but_not_through_the_toast() {
        let none = Gate::default();
        assert_eq!(wheel_target(&none, IN, Some(1), Some(2)), Some(1));
        assert_eq!(wheel_target(&none, IN, None, Some(2)), Some(2));
        let toast = Gate { toast: Some(R), ..Gate::default() };
        assert_eq!(wheel_target(&toast, IN, Some(1), Some(2)), None);
        assert_eq!(wheel_target(&toast, (20.0, 5.0), None, Some(2)), Some(2));
    }
}
