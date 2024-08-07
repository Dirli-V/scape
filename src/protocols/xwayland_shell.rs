use crate::focus::KeyboardFocusTarget;
use crate::grabs::{PointerMoveSurfaceGrab, PointerResizeSurfaceGrab, ResizeData, ResizeState};
use crate::shell::SurfaceData;
use crate::workspace_window::WorkspaceWindow;
use crate::{application_window::ApplicationWindow, State};
use smithay::delegate_xwayland_shell;
use smithay::desktop::Window;
use smithay::wayland::xwayland_shell::{XWaylandShellHandler, XWaylandShellState};
use smithay::{
    desktop::space::SpaceElement,
    input::pointer::Focus,
    utils::{Logical, Rectangle, SERIAL_COUNTER},
    wayland::{
        compositor::with_states,
        selection::data_device::{
            clear_data_device_selection, current_data_device_selection_userdata,
            request_data_device_client_selection, set_data_device_selection,
        },
        selection::{
            primary_selection::{
                clear_primary_selection, current_primary_selection_userdata,
                request_primary_client_selection, set_primary_selection,
            },
            SelectionTarget,
        },
    },
    xwayland::{
        xwm::{Reorder, ResizeEdge as X11ResizeEdge, XwmId},
        X11Surface, X11Wm, XwmHandler,
    },
};
use std::{cell::RefCell, os::fd::OwnedFd};
use tracing::{error, trace, warn};

#[derive(Debug, Default)]
struct OldGeometry(RefCell<Option<Rectangle<i32, Logical>>>);
impl OldGeometry {
    pub fn save(&self, geo: Rectangle<i32, Logical>) {
        *self.0.borrow_mut() = Some(geo);
    }

    pub fn restore(&self) -> Option<Rectangle<i32, Logical>> {
        self.0.borrow_mut().take()
    }
}

impl XwmHandler for State {
    fn xwm_state(&mut self, _xwm: XwmId) -> &mut X11Wm {
        self.xwayland_state.as_mut().unwrap().wm.as_mut().unwrap()
    }

    fn new_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        warn!("new window requested");
    }

    fn new_override_redirect_window(&mut self, _xwm: XwmId, _window: X11Surface) {
        warn!("new override redirect window requested");
    }

    fn map_window_request(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        if x11_surface.is_override_redirect() {
            // Don't do anything for override-redirect windows
            return;
        }

        if let Err(err) = x11_surface.set_mapped(true) {
            error!(?err, ?x11_surface, "Unable to map x11 surface");
            return;
        }

        let window = WorkspaceWindow::from(ApplicationWindow(Window::new_x11_window(
            x11_surface.clone(),
        )));
        // TODO: Handle multiple spaces
        let space_name = self.spaces.keys().next().unwrap().clone();
        let rect = self.place_window(&space_name, &window, true, None, false);
        let _bbox = self.spaces[&space_name].element_bbox(&window).unwrap();
        x11_surface.configure(Some(rect)).unwrap();
        window.set_ssd(!x11_surface.is_decorated());

        let keyboard = self.seat.as_ref().unwrap().get_keyboard().unwrap();
        let serial = SERIAL_COUNTER.next_serial();
        keyboard.set_focus(self, Some(window.into()), serial);
    }

    fn mapped_override_redirect_window(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        let location = x11_surface.geometry().loc;
        // TODO: Handle multiple spaces
        let space_name = self.spaces.keys().next().unwrap().clone();

        self.spaces.get_mut(&space_name).unwrap().map_element(
            WorkspaceWindow::from(ApplicationWindow(Window::new_x11_window(x11_surface))),
            // TODO: Check why wired starts with a crazy high value
            if location.x > 10_000 {
                (0, 0)
            } else {
                (location.x, location.y)
            },
            true,
        );
    }

    fn unmapped_window(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();
        space.unmap_elem(&window);
        if !x11_surface.is_override_redirect() {
            x11_surface.set_mapped(false).unwrap();
        }

        let maybe_window = space.elements().next_back().cloned();
        if let Some(window) = maybe_window {
            self.focus_window(window, &space_name);
        }
    }

    fn destroyed_window(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn configure_request(
        &mut self,
        _xwm: XwmId,
        window: X11Surface,
        _x: Option<i32>,
        _y: Option<i32>,
        w: Option<u32>,
        h: Option<u32>,
        _reorder: Option<Reorder>,
    ) {
        // we just set the new size, but don't let windows move themselves around freely
        let mut geo = window.geometry();
        if let Some(w) = w {
            geo.size.w = w as i32;
        }
        if let Some(h) = h {
            geo.size.h = h as i32;
        }
        let _ = window.configure(geo);
    }

    fn configure_notify(
        &mut self,
        _xwm: XwmId,
        x11_surface: X11Surface,
        geometry: Rectangle<i32, Logical>,
        _above: Option<u32>,
    ) {
        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();
        space.map_element(window, geometry.loc, false);
        // TODO: We don't properly handle the order of override-redirect windows here,
        //       they are always mapped top and then never reordered.
    }

    fn maximize_request(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        self.maximize_request_x11(&x11_surface);
    }

    fn unmaximize_request(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();

        x11_surface.set_maximized(false).unwrap();
        if let Some(old_geo) = x11_surface
            .user_data()
            .get::<OldGeometry>()
            .and_then(|data| data.restore())
        {
            x11_surface.configure(old_geo).unwrap();
            space.map_element(window, old_geo.loc, false);
        }
    }

    fn fullscreen_request(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();

        let outputs_for_window = space.outputs_for_element(&window);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| space.outputs().next())
            // Assumes that at least one output exists
            .expect("No outputs found");
        let geometry = space.output_geometry(output).unwrap();

        x11_surface.set_fullscreen(true).unwrap();
        window.set_ssd(false);
        x11_surface.configure(geometry).unwrap();
    }

    fn unfullscreen_request(&mut self, _xwm: XwmId, x11_surface: X11Surface) {
        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        // let space = self.spaces.get_mut(&space_name).unwrap();

        x11_surface.set_fullscreen(false).unwrap();
        window.set_ssd(!x11_surface.is_decorated());
        self.place_window(&space_name, &window, false, None, true);
    }

    fn resize_request(
        &mut self,
        _xwm: XwmId,
        x11_surface: X11Surface,
        _button: u32,
        edges: X11ResizeEdge,
    ) {
        // luckily anvil only supports one seat anyway...
        let start_data = self.pointer.as_ref().unwrap().grab_start_data().unwrap();

        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();

        let geometry = window.geometry();
        let loc = space.element_location(&window).unwrap();
        let (initial_window_location, initial_window_size) = (loc, geometry.size);

        with_states(&wl_surface, move |states| {
            states
                .data_map
                .get::<RefCell<SurfaceData>>()
                .unwrap()
                .borrow_mut()
                .resize_state = ResizeState::Resizing(ResizeData {
                edges: edges.into(),
                initial_window_location,
                initial_window_size,
            });
        });

        let grab = PointerResizeSurfaceGrab {
            start_data,
            window,
            space_name,
            edges: edges.into(),
            initial_window_location,
            initial_window_size,
            last_window_size: initial_window_size,
        };

        let pointer = self.pointer.clone().unwrap();
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }

    fn move_request(&mut self, _xwm: XwmId, window: X11Surface, _button: u32) {
        self.move_request_x11(&window)
    }

    fn allow_selection_access(&mut self, _xwm: XwmId, _selection: SelectionTarget) -> bool {
        if let Some(keyboard) = self.seat.as_ref().unwrap().get_keyboard() {
            // check that an X11 window is focused
            if let Some(KeyboardFocusTarget::Window(window)) = keyboard.current_focus() {
                return window.is_x11();
            }
        }
        false
    }

    fn send_selection(
        &mut self,
        _xwm: XwmId,
        selection: SelectionTarget,
        mime_type: String,
        fd: OwnedFd,
    ) {
        match selection {
            SelectionTarget::Clipboard => {
                if let Err(err) =
                    request_data_device_client_selection(self.seat.as_ref().unwrap(), mime_type, fd)
                {
                    error!(
                        ?err,
                        "Failed to request current wayland clipboard for Xwayland",
                    );
                }
            }
            SelectionTarget::Primary => {
                if let Err(err) =
                    request_primary_client_selection(self.seat.as_ref().unwrap(), mime_type, fd)
                {
                    error!(
                        ?err,
                        "Failed to request current wayland primary selection for Xwayland",
                    );
                }
            }
        }
    }

    fn new_selection(&mut self, _xwm: XwmId, selection: SelectionTarget, mime_types: Vec<String>) {
        trace!(?selection, ?mime_types, "Got Selection from X11",);
        // TODO check, that focused windows is X11 window before doing this
        match selection {
            SelectionTarget::Clipboard => set_data_device_selection(
                &self.display_handle,
                self.seat.as_ref().unwrap(),
                mime_types,
                (),
            ),
            SelectionTarget::Primary => set_primary_selection(
                &self.display_handle,
                self.seat.as_ref().unwrap(),
                mime_types,
                (),
            ),
        }
    }

    fn cleared_selection(&mut self, _xwm: XwmId, selection: SelectionTarget) {
        match selection {
            SelectionTarget::Clipboard => {
                if current_data_device_selection_userdata(self.seat.as_ref().unwrap()).is_some() {
                    clear_data_device_selection(&self.display_handle, self.seat.as_ref().unwrap())
                }
            }
            SelectionTarget::Primary => {
                if current_primary_selection_userdata(self.seat.as_ref().unwrap()).is_some() {
                    clear_primary_selection(&self.display_handle, self.seat.as_ref().unwrap())
                }
            }
        }
    }

    fn map_window_notify(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn property_notify(
        &mut self,
        _xwm: XwmId,
        _window: X11Surface,
        _property: smithay::xwayland::xwm::WmWindowProperty,
    ) {
    }

    fn minimize_request(&mut self, _xwm: XwmId, _window: X11Surface) {}

    fn unminimize_request(&mut self, _xwm: XwmId, _window: X11Surface) {}
}

impl State {
    pub fn maximize_request_x11(&mut self, x11_surface: &X11Surface) {
        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();

        let old_geo = space.element_bbox(&window).unwrap();
        let outputs_for_window = space.outputs_for_element(&window);
        let output = outputs_for_window
            .first()
            // The window hasn't been mapped yet, use the primary output instead
            .or_else(|| space.outputs().next())
            // Assumes that at least one output exists
            .expect("No outputs found");
        let geometry = space.output_geometry(output).unwrap();

        x11_surface.set_maximized(true).unwrap();
        x11_surface.configure(geometry).unwrap();
        x11_surface
            .user_data()
            .insert_if_missing(OldGeometry::default);
        x11_surface
            .user_data()
            .get::<OldGeometry>()
            .unwrap()
            .save(old_geo);
        space.map_element(window, geometry.loc, false);
    }

    pub fn move_request_x11(&mut self, x11_surface: &X11Surface) {
        // luckily anvil only supports one seat anyway...
        let Some(start_data) = self.pointer.as_ref().unwrap().grab_start_data() else {
            return;
        };

        let Some(wl_surface) = x11_surface.wl_surface() else {
            return;
        };
        let Some((window, space_name)) = self.window_and_space_for_surface(&wl_surface) else {
            return;
        };
        let space = self.spaces.get_mut(&space_name).unwrap();

        let mut initial_window_location = space.element_location(&window).unwrap();

        // If surface is maximized then unmaximize it
        if x11_surface.is_maximized() {
            x11_surface.set_maximized(false).unwrap();
            let pos = self.pointer_location();
            initial_window_location = (pos.x as i32, pos.y as i32).into();
            if let Some(old_geo) = x11_surface
                .user_data()
                .get::<OldGeometry>()
                .and_then(|data| data.restore())
            {
                x11_surface
                    .configure(Rectangle::from_loc_and_size(
                        initial_window_location,
                        old_geo.size,
                    ))
                    .unwrap();
            }
        }

        let grab = PointerMoveSurfaceGrab {
            start_data,
            window,
            space_name,
            initial_window_location,
        };

        let pointer = self.pointer.clone().unwrap();
        pointer.set_grab(self, grab, SERIAL_COUNTER.next_serial(), Focus::Clear);
    }
}

delegate_xwayland_shell!(State);

impl XWaylandShellHandler for State {
    fn xwayland_shell_state(&mut self) -> &mut XWaylandShellState {
        &mut self.xwayland_shell_state
    }
}
