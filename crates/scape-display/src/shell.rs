use crate::{
    grabs::ResizeState, state::ActiveSpace, workspace_window::WorkspaceWindow, ClientState, State,
};
use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    desktop::{
        layer_map_for_output, space::SpaceElement, PopupKind, PopupManager, Space,
        WindowSurfaceType,
    },
    input::pointer::{CursorImageStatus, CursorImageSurfaceData},
    reexports::{
        calloop::Interest,
        wayland_server::{
            protocol::{wl_buffer::WlBuffer, wl_surface::WlSurface},
            Client, Resource,
        },
    },
    utils::{Logical, Point, Rectangle, Size},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            add_blocker, add_pre_commit_hook, get_parent, is_sync_subsurface, with_states,
            with_surface_tree_upward, BufferAssignment, CompositorClientState, CompositorHandler,
            CompositorState, SurfaceAttributes, TraversalAction,
        },
        dmabuf::get_dmabuf,
        drm_syncobj::DrmSyncobjCachedState,
        shell::xdg::XdgToplevelSurfaceData,
    },
    xwayland::XWaylandClientData,
};
use std::cell::RefCell;
use tracing::info;

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &WlBuffer) {}
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.compositor_state
    }

    fn client_compositor_state<'a>(&self, client: &'a Client) -> &'a CompositorClientState {
        if let Some(state) = client.get_data::<XWaylandClientData>() {
            return &state.compositor_state;
        }
        if let Some(state) = client.get_data::<ClientState>() {
            return &state.compositor_state;
        }
        panic!("Unknown client data type")
    }

    fn new_surface(&mut self, surface: &WlSurface) {
        with_states(surface, |surface_data| {
            surface_data.data_map.insert_if_missing_threadsafe(|| {
                ActiveSpace(
                    self.spaces
                        .iter()
                        .next()
                        .expect("There should always be a space")
                        .0
                        .to_owned(),
                )
            })
        });

        add_pre_commit_hook::<Self, _>(surface, move |state, _dh, surface| {
            let mut acquire_point = None;
            let maybe_dmabuf = with_states(surface, |surface_data| {
                acquire_point.clone_from(
                    &surface_data
                        .cached_state
                        .get::<DrmSyncobjCachedState>()
                        .pending()
                        .acquire_point,
                );
                surface_data
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .pending()
                    .buffer
                    .as_ref()
                    .and_then(|assignment| match assignment {
                        BufferAssignment::NewBuffer(buffer) => get_dmabuf(buffer).cloned().ok(),
                        _ => None,
                    })
            });
            if let Some(dmabuf) = maybe_dmabuf {
                if let Some(acquire_point) = acquire_point {
                    if let Ok((blocker, source)) = acquire_point.generate_blocker() {
                        let client = surface.client().unwrap();
                        let res = state.loop_handle.insert_source(source, move |_, _, data| {
                            let dh = data.display_handle.clone();
                            data.client_compositor_state(&client)
                                .blocker_cleared(data, &dh);
                            Ok(())
                        });
                        if res.is_ok() {
                            add_blocker(surface, blocker);
                            return;
                        }
                    }
                }
                if let Ok((blocker, source)) = dmabuf.generate_blocker(Interest::READ) {
                    if let Some(client) = surface.client() {
                        let res = state.loop_handle.insert_source(source, move |_, _, data| {
                            let dh = data.display_handle.clone();
                            data.client_compositor_state(&client)
                                .blocker_cleared(data, &dh);
                            Ok(())
                        });
                        if res.is_ok() {
                            add_blocker(surface, blocker);
                        }
                    }
                }
            }
        });
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        self.backend_data.early_import(surface);

        if !is_sync_subsurface(surface) {
            let mut root = surface.clone();
            while let Some(parent) = get_parent(&root) {
                root = parent;
            }

            if let Some((window, space_name)) = self.window_and_space_for_surface(&root) {
                window.on_commit();

                if &root == surface {
                    let buffer_offset = with_states(surface, |states| {
                        states
                            .cached_state
                            .get::<SurfaceAttributes>()
                            .current()
                            .buffer_delta
                            .take()
                    });
                    if let Some(buffer_offset) = buffer_offset {
                        self.spaces.entry(space_name).and_modify(|space| {
                            let current_loc = space.element_location(&window).unwrap();
                            space.map_element(window, current_loc + buffer_offset, false);
                        });
                    }
                }
            }
        }
        self.popups.commit(surface);

        // TODO: Maybe move this to the cursor module
        if matches!(&self.cursor_state.status(), CursorImageStatus::Surface(cursor_surface) if cursor_surface == surface)
        {
            with_states(surface, |states| {
                let cursor_image_attributes = states.data_map.get::<CursorImageSurfaceData>();
                if let Some(mut cursor_image_attributes) =
                    cursor_image_attributes.map(|attrs| attrs.lock().unwrap())
                {
                    let buffer_delta = states
                        .cached_state
                        .get::<SurfaceAttributes>()
                        .current()
                        .buffer_delta
                        .take();
                    if let Some(buffer_delta) = buffer_delta {
                        tracing::trace!(hotspot = ?cursor_image_attributes.hotspot, ?buffer_delta, "decrementing cursor hotspot");
                        cursor_image_attributes.hotspot -= buffer_delta;
                    }
                }
            });
        }

        if matches!(&self.dnd_icon, Some(icon) if &icon.surface == surface) {
            let dnd_icon = self.dnd_icon.as_mut().unwrap();
            with_states(&dnd_icon.surface, |states| {
                let buffer_delta = states
                    .cached_state
                    .get::<SurfaceAttributes>()
                    .current()
                    .buffer_delta
                    .take()
                    .unwrap_or_default();
                tracing::trace!(offset = ?dnd_icon.offset, ?buffer_delta, "moving dnd offset");
                dnd_icon.offset += buffer_delta;
            });
        }

        let space_name = with_states(surface, |surface_data| {
            surface_data
                .data_map
                .get::<ActiveSpace>()
                .unwrap()
                .0
                .to_owned()
        });

        ensure_initial_configure(surface, &self.spaces[&space_name], &mut self.popups);

        // TODO: Only schedule the output that is affected
        self.backend_data.schedule_render();
    }
}

impl State {
    pub fn window_and_space_for_surface(
        &self,
        surface: &WlSurface,
    ) -> Option<(WorkspaceWindow, String)> {
        self.spaces
            .iter()
            .map(|(space_name, space)| {
                space
                    .elements()
                    .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
                    .map(|window| (window.to_owned(), space_name.clone()))
            })
            .next()?
    }

    pub fn space_of_window(&self, window: &WorkspaceWindow) -> Option<String> {
        self.spaces.iter().find_map(|(space_name, space)| {
            space
                .elements()
                .find(|w| *w == window)
                .map(|_| space_name.clone())
        })
    }
}

#[derive(Default)]
pub struct SurfaceData {
    pub geometry: Option<Rectangle<i32, Logical>>,
    pub resize_state: ResizeState,
}

// TODO: Try to find a better way to do this (this seems inefficient)
fn ensure_initial_configure(
    surface: &WlSurface,
    space: &Space<WorkspaceWindow>,
    popups: &mut PopupManager,
) {
    with_surface_tree_upward(
        surface,
        (),
        |_, _, _| TraversalAction::DoChildren(()),
        |_, states, _| {
            states
                .data_map
                .insert_if_missing(|| RefCell::new(SurfaceData::default()));
        },
        |_, _, _| true,
    );

    if let Some(window) = space
        .elements()
        .find(|window| window.wl_surface().map(|s| &*s == surface).unwrap_or(false))
        .cloned()
    {
        // send the initial configure if relevant
        if let Some(toplevel) = window.toplevel() {
            if !toplevel.is_initial_configure_sent() {
                toplevel.send_configure();
            }
        }

        with_states(surface, |states| {
            let mut data = states
                .data_map
                .get::<RefCell<SurfaceData>>()
                .unwrap()
                .borrow_mut();

            // Finish resizing.
            if let ResizeState::WaitingForCommit(_) = data.resize_state {
                data.resize_state = ResizeState::NotResizing;
            }
        });

        return;
    }

    if let Some(popup) = popups.find_popup(surface) {
        let popup = match popup {
            PopupKind::Xdg(ref popup) => popup,
            // Doesn't require configure
            PopupKind::InputMethod(ref input_popup) => {
                info!("PopupKind input method received {:?}", input_popup);
                return;
            }
        };
        if !popup.is_initial_configure_sent() {
            // NOTE: This should never fail as the initial configure is always
            // allowed.
            popup.send_configure().expect("initial configure failed");
        }

        return;
    };

    if let Some(output) = space.outputs().find(|o| {
        let map = layer_map_for_output(o);
        map.layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .is_some()
    }) {
        let initial_configure_sent = with_states(surface, |states| {
            if let Some(data) = states
                .data_map
                .get::<XdgToplevelSurfaceData>()
                .and_then(|d| d.try_lock().ok())
            {
                data.initial_configure_sent
            } else {
                // warn!("Unable to lock XdgToplevelSurfaceData in ensure_initial_configure 3");
                // TODO: Think of how to handle the case that there is no top level, but only a
                // toplevel layer like it is in `slurp`
                false
            }
        });

        let mut map = layer_map_for_output(output);

        // arrange the layers before sending the initial configure
        // to respect any size the client may have sent
        map.arrange();

        // send the initial configure if relevant
        if !initial_configure_sent {
            let layer = map
                .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
                .unwrap();

            layer.layer_surface().send_configure();
        }
    };
}

impl State {
    pub fn fixup_positions(&mut self, space_name: &str) {
        let space = self.spaces.get_mut(space_name).unwrap();
        // fixup outputs
        let mut offset = Point::<i32, Logical>::from((0, 0));
        for output in space.outputs().cloned().collect::<Vec<_>>().into_iter() {
            let size = space
                .output_geometry(&output)
                .map(|geo| geo.size)
                .unwrap_or_else(|| Size::from((0, 0)));
            space.map_output(&output, offset);
            layer_map_for_output(&output).arrange();
            offset.x += size.w;
        }

        // fixup windows
        let mut orphaned_windows = Vec::new();
        let outputs = space
            .outputs()
            .flat_map(|o| {
                let geo = space.output_geometry(o)?;
                let map = layer_map_for_output(o);
                let zone = map.non_exclusive_zone();
                Some(Rectangle::from_loc_and_size(geo.loc + zone.loc, zone.size))
            })
            .collect::<Vec<_>>();
        for window in space.elements() {
            let window_location = match space.element_location(window) {
                Some(loc) => loc,
                None => continue,
            };
            let geo_loc = window.bbox().loc + window_location;

            if !outputs.iter().any(|o_geo| o_geo.contains(geo_loc)) {
                orphaned_windows.push(window.clone());
            }
        }
        for window in orphaned_windows.into_iter() {
            self.place_window(space_name, &window, false, None, true);
        }
    }
}
