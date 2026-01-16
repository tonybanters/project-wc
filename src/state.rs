use smithay::{
    desktop::{PopupManager, Space, Window, WindowSurfaceType},
    input::{Seat, SeatState, pointer::PointerHandle},
    reexports::{
        calloop::{Interest, LoopHandle, LoopSignal, Mode, PostAction, generic::Generic},
        wayland_server::{
            Display, DisplayHandle,
            backend::{ClientData, ClientId, DisconnectReason},
            protocol::wl_surface::WlSurface,
        },
    },
    utils::{Logical, Point},
    wayland::{
        compositor::{CompositorClientState, CompositorState},
        output::OutputManagerState,
        selection::{data_device::DataDeviceState, primary_selection::PrimarySelectionState},
        shell::{wlr_layer::WlrLayerShellState, xdg::XdgShellState},
        shm::ShmState,
        socket::ListeningSocketSource,
    },
};
use std::{env, ffi::OsString, sync::Arc};

use crate::{
    CompositorError,
    backend::{Backend, udev::Udev, winit::Winit},
    layout::{GapConfig, LayoutBox, LayoutType},
    protocols::wlr_screencopy::{Screencopy, ScreencopyManagerState},
};

pub struct State {
    pub backend: Backend,
    pub projectwc: ProjectWC,
}

impl State {
    // TODO: Review error type
    pub fn new(
        event_loop: LoopHandle<'static, State>,
        loop_signal: LoopSignal,
        display: Display<State>,
    ) -> Option<Self> {
        let has_display =
            env::var_os("WAYLAND_DISPLAY").is_some() || env::var_os("DISPLAY").is_some();

        let mut projectwc = ProjectWC::new(display, event_loop.clone(), loop_signal);
        let backend = if has_display {
            let winit = Winit::new(&mut projectwc).ok()?;
            Backend::Winit(winit)
        } else {
            let udev = Udev::new(event_loop, &mut projectwc)?;
            Backend::Udev(udev)
        };

        let state = State { backend, projectwc };

        Some(state)
    }
}

pub struct ProjectWC {
    pub display_handle: DisplayHandle,
    pub loop_handle: LoopHandle<'static, State>,
    pub loop_signal: LoopSignal,

    pub space: Space<Window>,
    pub seat: Seat<State>,
    pub layout: LayoutBox,
    pub socket_name: OsString,
    pub start_time: std::time::Instant,

    // smithay state
    pub compositor_state: CompositorState,
    pub xdg_shell_state: XdgShellState,
    pub shm_state: ShmState,
    pub output_manager_state: OutputManagerState,
    pub data_device_state: DataDeviceState,
    pub seat_state: SeatState<State>,
    pub popups: PopupManager,
    pub primary_selection_state: PrimarySelectionState,
    pub layer_shell_state: WlrLayerShellState,
    pub screencopy_state: ScreencopyManagerState,

    pub pointer_location: Point<f64, Logical>,
    pub pending_screencopy: Option<Screencopy>,
}

impl ProjectWC {
    pub fn new(
        display: Display<State>,
        loop_handle: LoopHandle<'static, State>,
        loop_signal: LoopSignal,
    ) -> Self {
        let start_time = std::time::Instant::now();

        let display_handle = display.handle();

        // State
        let compositor_state = CompositorState::new::<State>(&display_handle);
        let xdg_shell_state = XdgShellState::new::<State>(&display_handle);
        let shm_state = ShmState::new::<State>(&display_handle, vec![]);
        let output_manager_state =
            OutputManagerState::new_with_xdg_output::<State>(&display_handle);
        let data_device_state = DataDeviceState::new::<State>(&display_handle);
        let popups = PopupManager::default();
        let primary_selection_state = PrimarySelectionState::new::<State>(&display_handle);
        let layer_shell_state = WlrLayerShellState::new::<State>(&display_handle);
        let screencopy_state = ScreencopyManagerState::new::<State, _>(&display_handle, |_| true);
        let mut seat_state = SeatState::new();

        // TODO: Use backends's `seat_name`
        let mut seat = seat_state.new_wl_seat(&display_handle, "winit");
        seat.add_keyboard(Default::default(), 200, 25)
            .expect("failed to add keyboard");
        seat.add_pointer();

        let space = Space::default();

        let socket_name = init_wayland_listener(display, &loop_handle);

        // TODO: Get a brain
        let layout = LayoutType::from_str("tiling").unwrap().new();

        Self {
            display_handle,
            loop_handle,
            loop_signal,

            space,
            layout,
            seat,
            socket_name,
            start_time,

            compositor_state,
            xdg_shell_state,
            shm_state,
            output_manager_state,
            data_device_state,
            seat_state,
            popups,
            primary_selection_state,
            layer_shell_state,
            screencopy_state,

            pointer_location: Point::from((0.0, 0.0)),
            pending_screencopy: None,
        }
    }

    pub fn apply_layout(&mut self) -> Result<(), CompositorError> {
        let windows: Vec<smithay::desktop::Window> = self.space.elements().cloned().collect();
        if windows.is_empty() {
            return Ok(());
        }

        let output = self
            .space
            .outputs()
            .next()
            .cloned()
            .ok_or_else(|| CompositorError::Backend("no output".into()))?;

        let out_geo = self
            .space
            .output_geometry(&output)
            .ok_or_else(|| CompositorError::Backend("no output geometry".into()))?;

        let gaps = GapConfig {
            outer_horizontal: 20,
            outer_vertical: 20,
            inner_horizontal: 10,
            inner_vertical: 10,
        };

        let master_factor: f32 = 0.55;
        let num_master: i32 = 1;
        let smartgaps_enabled: bool = true;

        let geometries = self.layout.arrange(
            &windows,
            out_geo.size.w as u32,
            out_geo.size.h as u32,
            &gaps,
            master_factor,
            num_master,
            smartgaps_enabled,
        );

        for (window, geom) in windows.into_iter().zip(geometries.into_iter()) {
            let loc = Point::<i32, Logical>::from((
                out_geo.loc.x + geom.x_coordinate,
                out_geo.loc.y + geom.y_coordinate,
            ));

            if let Some(toplevel) = window.toplevel() {
                toplevel.with_pending_state(|state| {
                    state.size = Some((geom.width as i32, geom.height as i32).into());
                });
                toplevel.send_pending_configure();
            }

            self.space.map_element(window, loc, false);
        }

        Ok(())
    }

    pub fn window_for_surface(&self, surface: &WlSurface) -> Option<Window> {
        self.space
            .elements()
            .find(|window| {
                window
                    .toplevel()
                    .is_some_and(|tl| tl.wl_surface() == surface)
            })
            .cloned()
    }

    pub fn window_under_pointer(&self) -> Option<(Window, Point<i32, Logical>)> {
        self.space
            .element_under(self.pointer_location)
            .map(|(w, p)| (w.clone(), p))
    }

    pub fn surface_under_pointer(&self) -> Option<(WlSurface, Point<f64, Logical>)> {
        let position = self.pointer_location;
        self.space
            .element_under(position)
            .and_then(|(window, location)| {
                window
                    .surface_under(position - location.to_f64(), WindowSurfaceType::ALL)
                    .map(|(s, p)| (s, (p + location).to_f64()))
            })
    }

    pub fn pointer(&self) -> PointerHandle<State> {
        self.seat.get_pointer().expect("pointer not initialized")
    }
}

pub fn init_wayland_listener(
    display: Display<State>,
    loop_handle: &LoopHandle<'static, State>,
) -> OsString {
    let listening_socket = ListeningSocketSource::new_auto().expect("failed to create socket");
    let socket_name = listening_socket.socket_name().to_os_string();

    loop_handle
        .insert_source(listening_socket, move |client_stream, _, state| {
            state
                .projectwc
                .display_handle
                .insert_client(client_stream, Arc::new(ClientState::default()))
                .expect("failed to insert client");
        })
        .expect("failed to init wayland listener");

    loop_handle
        .insert_source(
            Generic::new(display, Interest::READ, Mode::Level),
            move |_, display, state| {
                // Safety: we don't drop the display
                unsafe {
                    display.get_mut().dispatch_clients(state).unwrap();
                }
                Ok(PostAction::Continue)
            },
        )
        .expect("failed to init display event source");

    socket_name
}

#[derive(Default)]
pub struct ClientState {
    pub compositor_state: CompositorClientState,
}

impl ClientData for ClientState {
    fn initialized(&self, _client_id: ClientId) {}
    fn disconnected(&self, _client_id: ClientId, _reason: DisconnectReason) {}
}
