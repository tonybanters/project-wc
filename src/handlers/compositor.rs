use smithay::{
    backend::renderer::utils::on_commit_buffer_handler,
    delegate_compositor, delegate_shm,
    reexports::wayland_server::protocol::{wl_buffer, wl_surface::WlSurface},
    wayland::{
        buffer::BufferHandler,
        compositor::{
            CompositorClientState, CompositorHandler, CompositorState, get_parent,
            is_sync_subsurface,
        },
        shm::{ShmHandler, ShmState},
    },
};

use crate::{
    grabs::resize_grab,
    handlers::{layer_shell, xdg_shell},
    state::{ClientState, State},
};

impl CompositorHandler for State {
    fn compositor_state(&mut self) -> &mut CompositorState {
        &mut self.projectwc.compositor_state
    }

    fn client_compositor_state<'a>(
        &self,
        client: &'a smithay::reexports::wayland_server::Client,
    ) -> &'a CompositorClientState {
        &client.get_data::<ClientState>().unwrap().compositor_state
    }

    fn commit(&mut self, surface: &WlSurface) {
        on_commit_buffer_handler::<Self>(surface);
        if !is_sync_subsurface(surface) {
            let mut root_surface = surface.clone();
            while let Some(parent) = get_parent(&root_surface) {
                root_surface = parent;
            }

            if let Some(window) = self.projectwc.window_for_surface(&root_surface) {
                window.on_commit();
            }
        }

        xdg_shell::handle_commit(&mut self.projectwc.popups, &self.projectwc.space, surface);
        resize_grab::handle_commit(&mut self.projectwc.space, surface);
        layer_shell::handle_commit(&mut self.projectwc.space, surface);
    }
}

impl BufferHandler for State {
    fn buffer_destroyed(&mut self, _buffer: &wl_buffer::WlBuffer) {}
}

impl ShmHandler for State {
    fn shm_state(&self) -> &ShmState {
        &self.projectwc.shm_state
    }
}

delegate_shm!(State);
delegate_compositor!(State);
