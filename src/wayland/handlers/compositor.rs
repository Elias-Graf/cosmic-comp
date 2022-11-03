// SPDX-License-Identifier: GPL-3.0-only

use crate::{
    state::BackendData,
    utils::prelude::*,
    wayland::{
        handlers::screencopy::UserdataExt,
        protocols::screencopy::{BufferParams, Session as ScreencopySession, SessionType},
    },
};
use smithay::{
    backend::renderer::utils::{on_commit_buffer_handler, with_renderer_surface_state},
    delegate_compositor,
    desktop::{layer_map_for_output, Kind, LayerSurface, PopupKind, WindowSurfaceType},
    reexports::wayland_server::protocol::wl_surface::WlSurface,
    utils::IsAlive,
    wayland::{
        compositor::{with_states, CompositorHandler, CompositorState},
        shell::{
            wlr_layer::LayerSurfaceAttributes,
            xdg::{
                ToplevelSurface, XdgPopupSurfaceRoleAttributes, XdgToplevelSurfaceRoleAttributes,
            },
        },
    },
};
use std::sync::Mutex;

use super::screencopy::{self, PendingScreencopyBuffers};

impl State {
    fn early_import_surface(&mut self, surface: &WlSurface) {
        let mut import_nodes = std::collections::HashSet::new();
        let dh = &self.common.display_handle;
        for output in self.common.shell.visible_outputs_for_surface(&surface) {
            if let BackendData::Kms(ref mut kms_state) = &mut self.backend {
                if let Some(target) = kms_state.target_node_for_output(&output) {
                    if import_nodes.insert(target) {
                        kms_state.try_early_import(
                            dh,
                            surface,
                            &output,
                            target,
                            &self.common.shell,
                        );
                    }
                }
            }
        }
    }

    fn toplevel_ensure_initial_configure(&mut self, toplevel: &ToplevelSurface) -> bool {
        // send the initial configure if relevant
        let initial_configure_sent = with_states(toplevel.wl_surface(), |states| {
            states
                .data_map
                .get::<Mutex<XdgToplevelSurfaceRoleAttributes>>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            // TODO: query expected size from shell (without inserting and mapping)
            toplevel.with_pending_state(|states| states.size = None);
            toplevel.send_configure();
        }
        initial_configure_sent
    }

    fn xdg_popup_ensure_initial_configure(&mut self, popup: &PopupKind) {
        let PopupKind::Xdg(ref popup) = popup;
        let initial_configure_sent = with_states(popup.wl_surface(), |states| {
            states
                .data_map
                .get::<Mutex<XdgPopupSurfaceRoleAttributes>>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            // NOTE: This should never fail as the initial configure is always
            // allowed.
            popup.send_configure().expect("initial configure failed");
        }
    }

    fn layer_surface_ensure_inital_configure(&mut self, surface: &LayerSurface) -> bool {
        // send the initial configure if relevant
        let initial_configure_sent = with_states(surface.wl_surface(), |states| {
            states
                .data_map
                .get::<Mutex<LayerSurfaceAttributes>>()
                .unwrap()
                .lock()
                .unwrap()
                .initial_configure_sent
        });
        if !initial_configure_sent {
            // compute initial dimensions by mapping
            Shell::map_layer(self, &surface);
            // this will also send a configure
        }
        initial_configure_sent
    }
}

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.common.compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        // first load the buffer for various smithay helper functions
        on_commit_buffer_handler(surface);

        // then handle initial configure events and map windows if necessary
        if let Some((window, seat)) = self
            .common
            .shell
            .pending_windows
            .iter()
            .find(|(window, _)| window.toplevel().wl_surface() == surface)
            .cloned()
        {
            match window.toplevel() {
                Kind::Xdg(toplevel) => {
                    if self.toplevel_ensure_initial_configure(&toplevel)
                        && with_renderer_surface_state(&surface, |state| {
                            state.wl_buffer().is_some()
                        })
                    {
                        let output = seat.active_output();
                        Shell::map_window(self, &window, &output);
                    } else {
                        return;
                    }
                }
            }
        }

        if let Some((layer_surface, _, _)) = self
            .common
            .shell
            .pending_layers
            .iter()
            .find(|(layer_surface, _, _)| layer_surface.wl_surface() == surface)
            .cloned()
        {
            if !self.layer_surface_ensure_inital_configure(&layer_surface) {
                return;
            }
        };

        if let Some(popup) = self.common.shell.popups.find_popup(surface) {
            self.xdg_popup_ensure_initial_configure(&popup);
        }

        // at last handle some special cases, like grabs and changing layer surfaces

        // If we would re-position the window inside the grab we would get a weird jittery animation.
        // We only want to resize once the client has acknoledged & commited the new size,
        // so we need to carefully track the state through different handlers.
        if let Some(element) = self.common.shell.element_for_surface(surface).cloned() {
            if let Some(workspace) = self.common.shell.space_for_mut(&element) {
                crate::shell::layout::floating::ResizeSurfaceGrab::apply_resize_to_location(
                    element.clone(),
                    workspace,
                );
                workspace.commit(surface);
            }

            // handle window screencopy sessions
            let active = element.active_window();
            if active.toplevel().wl_surface() == surface {
                for (session, params) in active.pending_buffers() {
                    let window = active.clone();
                    self.common.event_loop_handle.insert_idle(move |data| {
                        if !session.alive() {
                            return;
                        }

                        match screencopy::render_window_to_buffer(
                            &mut data.state,
                            &session,
                            params.clone(),
                            &window,
                        ) {
                            // rendering yielded no damage, buffer is still pending
                            Ok(false) => data.state.common.still_pending(session, params),
                            Ok(true) => {} // success
                            Err((reason, err)) => {
                                slog_scope::warn!("Screencopy session failed: {}", err);
                                session.failed(reason);
                            }
                        }
                    });
                }
            }
        }

        // We need to know every potential output for importing to the right gpu and scheduling a render,
        // so call this only after every potential surface map operation has been done.
        self.early_import_surface(surface);

        // and refresh smithays internal state
        self.common.shell.popups.commit(surface);

        // re-arrange layer-surfaces (commits may change size and positioning)
        if let Some(output) = self.common.shell.outputs().find(|o| {
            let map = layer_map_for_output(o);
            map.layer_for_surface(surface, WindowSurfaceType::ALL)
                .is_some()
        }) {
            layer_map_for_output(output).arrange();
        }

        // here we store additional workspace_sessions, we should handle, when rendering the corresponding output anyway
        let mut scheduled_sessions: Option<Vec<(ScreencopySession, BufferParams)>> = None;

        // lets check which workspaces this surface belongs to
        let active_spaces = self
            .common
            .shell
            .outputs()
            .map(|o| (o.clone(), self.common.shell.active_space(o).handle.clone()))
            .collect::<Vec<_>>();
        for (handle, output) in self.common.shell.workspaces_for_surface(surface) {
            let workspace = self.common.shell.space_for_handle_mut(&handle).unwrap();
            if !workspace.pending_buffers.is_empty() {
                // TODO: replace with drain_filter....
                let mut i = 0;
                while i < workspace.pending_buffers.len() {
                    if let SessionType::Workspace(o, w) =
                        workspace.pending_buffers[i].0.session_type()
                    {
                        if active_spaces.contains(&(o.clone(), w)) {
                            // surface is on an active workspace/output combo, add to workspace_sessions
                            let (session, params) = workspace.pending_buffers.remove(i);
                            scheduled_sessions
                                .get_or_insert_with(Vec::new)
                                .push((session, params));
                        } else if handle == w && output == o {
                            // surface is visible on an offscreen workspace session, schedule a new render
                            let (session, params) = workspace.pending_buffers.remove(i);
                            let output = output.clone();
                            self.common.event_loop_handle.insert_idle(move |data| {
                                if !session.alive() {
                                    return;
                                }
                                match screencopy::render_workspace_to_buffer(
                                    &mut data.state,
                                    &session,
                                    params.clone(),
                                    &output,
                                    &handle,
                                ) {
                                    Ok(false) => {
                                        // rendering yielded no new damage, buffer still pending
                                        data.state.common.still_pending(session, params);
                                    }
                                    Ok(true) => {}
                                    Err((reason, err)) => {
                                        slog_scope::warn!("Screencopy session failed: {}", err);
                                        session.failed(reason);
                                    }
                                }
                            });
                        } else {
                            i += 1;
                        }
                    } else {
                        unreachable!();
                    }
                }
            }
        }

        // schedule a new render
        for output in self.common.shell.visible_outputs_for_surface(surface) {
            if let Some(sessions) = output.user_data().get::<PendingScreencopyBuffers>() {
                scheduled_sessions
                    .get_or_insert_with(Vec::new)
                    .extend(sessions.borrow_mut().drain(..));
            }

            self.backend.schedule_render(
                &self.common.event_loop_handle,
                &output,
                scheduled_sessions.as_ref().map(|sessions| {
                    sessions
                        .iter()
                        .filter(|(s, _)| match s.session_type() {
                            SessionType::Output(o) | SessionType::Workspace(o, _)
                                if o == output =>
                            {
                                true
                            }
                            _ => false,
                        })
                        .cloned()
                        .collect::<Vec<_>>()
                }),
            );
        }
    }
}

delegate_compositor!(State);
