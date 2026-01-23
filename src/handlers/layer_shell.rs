use crate::state::State;
use smithay::delegate_layer_shell;
use smithay::desktop::{LayerSurface, Space, Window, WindowSurfaceType, layer_map_for_output};
use smithay::output::Output;
use smithay::reexports::wayland_server::protocol::wl_output::WlOutput;
use smithay::reexports::wayland_server::protocol::wl_surface::WlSurface;
use smithay::wayland::compositor;
use smithay::wayland::shell::wlr_layer::{
    Layer, LayerSurface as WlrLayerSurface, LayerSurfaceData, WlrLayerShellHandler,
    WlrLayerShellState,
};
use smithay::wayland::shell::xdg::PopupSurface;

impl WlrLayerShellHandler for State {
    fn shell_state(&mut self) -> &mut WlrLayerShellState {
        &mut self.projectwc.layer_shell_state
    }

    fn new_layer_surface(
        &mut self,
        surface: WlrLayerSurface,
        wl_output: Option<WlOutput>,
        _layer: Layer,
        namespace: String,
    ) {
        let output = wl_output
            .as_ref()
            .and_then(Output::from_resource)
            .or_else(|| self.projectwc.space.outputs().next().cloned());

        let Some(output) = output else {
            tracing::warn!(namespace, "no output for new layer surface");
            return;
        };

        let layer_surface = LayerSurface::new(surface, namespace);
        let mut layer_map = layer_map_for_output(&output);
        if let Err(err) = layer_map.map_layer(&layer_surface) {
            tracing::warn!("failed to map layer surface: {err:?}");
        }
    }

    fn layer_destroyed(&mut self, surface: WlrLayerSurface) {
        if let Some((mut map, layer)) = self.projectwc.space.outputs().find_map(|output| {
            let map = layer_map_for_output(output);
            let layer = map
                .layers()
                .find(|layer| layer.layer_surface() == &surface)
                .cloned();

            layer.map(|layer| (map, layer))
        }) {
            map.unmap_layer(&layer);
        }
    }

    fn new_popup(&mut self, _parent: WlrLayerSurface, popup: PopupSurface) {
        self.unconstrain_popup(&popup);
    }
}
delegate_layer_shell!(State);

/// Should be called on `WlSurface::commit`
pub fn handle_commit(space: &mut Space<Window>, surface: &WlSurface) {
    for output in space.outputs() {
        let mut layer_map = layer_map_for_output(output);
        if let Some(layer) = layer_map
            .layer_for_surface(surface, WindowSurfaceType::TOPLEVEL)
            .cloned()
        {
            layer_map.arrange();

            let initial_configure_sent = compositor::with_states(surface, |states| {
                states
                    .data_map
                    .get::<LayerSurfaceData>()
                    .map(|data| data.lock().unwrap().initial_configure_sent)
                    .unwrap_or(true)
            });
            if !initial_configure_sent {
                layer.layer_surface().send_configure();
            }
            break;
        }
    }
}
