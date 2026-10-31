//! Where a pointer event goes, given what covers the window: the modal
//! overlays (palette, menu, picker, confirmation) and the toast. Pure
//! functions, so they test without a window.

use ghostrealm_core::{PaneId, Rect, SurfaceId, VtabId};

use crate::app_state::PICKER_SID;
use crate::plugin::rect_contains;
use crate::toasts::ToastHit;

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
    /// Fold or unfold the sidebar's directory group this workspace is in.
    ToggleGroup(VtabId),
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
    /// A part of the toast with this id.
    Toast(u64, ToastHit),
}

impl ButtonAction {
    /// One of the picker's own buttons (its confirm and cancel, and its
    /// browser's).
    fn in_picker(self) -> bool {
        matches!(self, ButtonAction::PickerConfirm | ButtonAction::PickerCancel | ButtonAction::View(PICKER_SID, _))
    }

    fn in_toast(self) -> bool {
        matches!(self, ButtonAction::Toast(..))
    }

    /// One of the confirmation's buttons.
    fn in_confirm(self) -> bool {
        matches!(self, ButtonAction::ConfirmAccept | ButtonAction::ConfirmCancel | ButtonAction::ConfirmDiscard)
    }
}

/// What covers the window this frame. The toast draws over the palette,
/// picker and confirmation, and under a menu.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct Gate {
    /// A menu is open, with its panel as last drawn: it owns the pointer,
    /// over the toast too.
    pub menu: Option<Rect>,
    /// The palette is open: it owns the pointer outside the toast.
    pub palette: bool,
    /// The floating picker is open: only its buttons (and the toast's) are
    /// live.
    pub picker: bool,
    /// A confirmation is open: only its buttons (and the toast's) are live.
    pub confirm: bool,
    /// The toast's rect, as last drawn.
    pub toast: Option<Rect>,
}

/// Whether the toast covers `pos`: nothing under it takes the pointer.
pub(crate) fn covered(gate: &Gate, (x, y): (f32, f32)) -> bool {
    gate.toast.is_some_and(|r| rect_contains(r, x, y))
}

/// The button at `pos`, if any. A menu owns the pointer; under the toast
/// only the toast's buttons are live (view buttons under it come first in
/// paint order); elsewhere the palette owns the pointer, and while a
/// confirmation or the picker is open only its buttons are live. The
/// sidebar toggle wins any overlap (it can float over a pane's tab strip when
/// the sidebar is hidden); otherwise the first button in paint order takes
/// the hit.
pub(crate) fn button_at(gate: &Gate, buttons: &[(Rect, ButtonAction)], (x, y): (f32, f32)) -> Option<ButtonAction> {
    if gate.menu.is_some() {
        return None;
    }
    let toast = covered(gate, (x, y));
    if gate.palette && !toast {
        return None;
    }
    let mut hit = None;
    for &(r, action) in buttons {
        let live = if toast {
            action.in_toast()
        } else if gate.confirm {
            action.in_confirm()
        } else {
            !gate.picker || action.in_picker()
        };
        if !live {
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

/// The index in `buttons` of the one to light up under `pos`: under the
/// toast only one of its own, so nothing beneath it lights up.
pub(crate) fn hover_index(gate: &Gate, buttons: &[(Rect, ButtonAction)], (x, y): (f32, f32)) -> Option<usize> {
    let toast = covered(gate, (x, y));
    buttons.iter().position(|(r, a)| (!toast || a.in_toast()) && rect_contains(*r, x, y))
}

/// Where a right press goes.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum RightPress {
    /// Onto a menu's panel: it closes the menu and goes no further, whatever
    /// is under the menu (the menu owns the pointer, over the toast and the
    /// picker too).
    Menu,
    /// Nowhere: the picker or a confirmation keeps the pointer (a menu would
    /// open beneath it, and its actions run under an unanswered question).
    Kept,
    /// Onto the toast, off any menu: it closes a menu, and nothing beneath
    /// the toast (the palette, a view) hears it.
    Toast,
    /// Beneath the overlays, off any menu and the toast: the palette and a
    /// menu close, and the sidebar or the view under the pointer takes it.
    Through,
}

/// Where a right press at `pos` goes.
pub(crate) fn right_press(gate: &Gate, pos: (f32, f32)) -> RightPress {
    if gate.menu.is_some_and(|r| rect_contains(r, pos.0, pos.1)) {
        RightPress::Menu
    } else if gate.picker || gate.confirm {
        RightPress::Kept
    } else if covered(gate, pos) {
        RightPress::Toast
    } else {
        RightPress::Through
    }
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

    /// A right press on the toast reaches nothing beneath it, the palette
    /// included; off it, it goes through unless a modal keeps the pointer.
    #[test]
    fn a_right_press_on_the_toast_goes_no_further() {
        let gate = Gate { palette: true, toast: Some(R), ..Gate::default() };
        assert_eq!(right_press(&gate, IN), RightPress::Toast);
        assert_eq!(right_press(&gate, (20.0, 5.0)), RightPress::Through);
        let off = Rect { x: 20.0, ..R };
        let menu = Gate { menu: Some(off), ..gate };
        assert_eq!(right_press(&menu, IN), RightPress::Toast, "off the menu");
        let menu = Gate { menu: Some(Rect { x: 4.0, ..R }), ..gate };
        assert_eq!(right_press(&menu, IN), RightPress::Menu, "the menu over the toast owns the press");
        assert_eq!(right_press(&menu, (2.0, 5.0)), RightPress::Toast, "the toast beside the menu");
        let picker = Gate { picker: true, ..gate };
        assert_eq!(right_press(&picker, IN), RightPress::Kept);
        assert_eq!(right_press(&picker, (20.0, 5.0)), RightPress::Kept);
        let confirm = Gate { confirm: true, ..gate };
        assert_eq!(right_press(&confirm, IN), RightPress::Kept);
        assert_eq!(right_press(&Gate::default(), IN), RightPress::Through);
    }

    /// A right press on a menu's panel is the menu's, with or without the
    /// toast or a modal under it; beside the menu it goes as if it were shut.
    #[test]
    fn a_right_press_on_a_menu_is_the_menus() {
        let menu = Gate { menu: Some(R), ..Gate::default() };
        assert_eq!(right_press(&menu, IN), RightPress::Menu, "no toast under it");
        assert_eq!(right_press(&menu, (20.0, 5.0)), RightPress::Through);
        let toast = Gate { toast: Some(R), ..menu };
        assert_eq!(right_press(&toast, IN), RightPress::Menu, "the toast under it");
        let picker = Gate { picker: true, ..menu };
        assert_eq!(right_press(&picker, IN), RightPress::Menu, "the picker under it");
        assert_eq!(right_press(&picker, (20.0, 5.0)), RightPress::Kept);
    }

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
        for owner in [Gate { menu: Some(R), ..Gate::default() }, Gate { palette: true, ..Gate::default() }] {
            assert_eq!(button_at(&owner, &buttons, IN), None);
        }
        let picker = Gate { picker: true, ..Gate::default() };
        assert_eq!(button_at(&picker, &buttons, IN), Some(ButtonAction::PickerConfirm));
        let confirm = Gate { picker: true, confirm: true, ..Gate::default() };
        assert_eq!(button_at(&confirm, &buttons, IN), Some(ButtonAction::ConfirmCancel));
    }

    /// The toast draws over the palette, picker and confirmation: its
    /// buttons stay live over them, and nothing of theirs under it is.
    /// A menu draws over the toast and owns the pointer.
    #[test]
    fn the_toast_takes_the_pointer_over_a_modal_but_not_over_a_menu() {
        let toast = Rect { x: 0.0, y: 0.0, w: 10.0, h: 6.0 };
        let close = Rect { x: 6.0, y: 2.0, w: 4.0, h: 4.0 };
        let buttons = [
            (R, ButtonAction::PickerCancel),
            (R, ButtonAction::ConfirmCancel),
            (close, ButtonAction::Toast(7, ToastHit::Dismiss)),
        ];
        let (on_close, on_body, below) = ((8.0, 4.0), (2.0, 2.0), (2.0, 8.0));
        let dismiss = Some(ButtonAction::Toast(7, ToastHit::Dismiss));
        let modals = [
            (Gate { palette: true, ..Gate::default() }, None),
            (Gate { picker: true, ..Gate::default() }, Some(ButtonAction::PickerCancel)),
            (Gate { picker: true, confirm: true, ..Gate::default() }, Some(ButtonAction::ConfirmCancel)),
        ];
        for (gate, beneath) in modals {
            let gate = Gate { toast: Some(toast), ..gate };
            assert_eq!(button_at(&gate, &buttons, on_close), dismiss, "{gate:?}");
            assert_eq!(button_at(&gate, &buttons, on_body), None, "{gate:?}");
            assert_eq!(button_at(&gate, &buttons, below), beneath, "{gate:?}");
        }
        let menu = Gate { menu: Some(toast), toast: Some(toast), ..Gate::default() };
        assert_eq!(button_at(&menu, &buttons, on_close), None);
    }

    #[test]
    fn the_toast_covers_only_its_rect() {
        let gate = Gate { toast: Some(R), ..Gate::default() };
        assert!(covered(&gate, IN));
        assert!(!covered(&gate, (20.0, 5.0)));
        assert!(!covered(&Gate::default(), IN));
    }

    #[test]
    fn under_the_toast_only_its_buttons_take_the_pointer() {
        let close = Rect { x: 6.0, y: 2.0, w: 4.0, h: 4.0 };
        let toast = Rect { x: 0.0, y: 0.0, w: 10.0, h: 6.0 };
        // A view button under the toast, registered (painted) first.
        let buttons = [
            (R, ButtonAction::View(SurfaceId(1), 0)),
            (R, ButtonAction::ToggleSidebar),
            (close, ButtonAction::Toast(7, ToastHit::Dismiss)),
        ];
        let gate = Gate { toast: Some(toast), ..Gate::default() };
        let on_close = (8.0, 4.0);
        assert_eq!(button_at(&gate, &buttons, on_close), Some(ButtonAction::Toast(7, ToastHit::Dismiss)));
        assert_eq!(hover_index(&gate, &buttons, on_close), Some(2));
        // The toast's body: nothing under it.
        assert_eq!(button_at(&gate, &buttons, (2.0, 2.0)), None);
        assert_eq!(hover_index(&gate, &buttons, (2.0, 2.0)), None);
        assert!(covered(&gate, (2.0, 2.0)));
        assert_eq!(wheel_target(&gate, (2.0, 2.0), Some(1), Some(2)), None);
        // Below it, everything is as before.
        assert_eq!(button_at(&gate, &buttons, (2.0, 8.0)), Some(ButtonAction::ToggleSidebar));
        assert_eq!(hover_index(&gate, &buttons, (2.0, 8.0)), Some(0));
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
