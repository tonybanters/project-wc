mod compositor;
mod layer_shell;
mod xdg_shell;

use smithay::{
    backend::drm::DrmNode,
    delegate_data_device, delegate_drm_lease, delegate_output, delegate_primary_selection,
    delegate_seat,
    input::{
        Seat, SeatHandler, SeatState,
        dnd::{DnDGrab, DndGrabHandler, GrabType},
        pointer::{CursorImageStatus, Focus},
    },
    reexports::wayland_server::{Resource, protocol::wl_surface::WlSurface},
    wayland::{
        drm_lease::{
            DrmLeaseBuilder, DrmLeaseHandler, DrmLeaseRequest, DrmLeaseState, LeaseRejected,
        },
        output::OutputHandler,
        selection::{
            SelectionHandler,
            data_device::{
                DataDeviceHandler, DataDeviceState, WaylandDndGrabHandler, set_data_device_focus,
            },
            primary_selection::{
                PrimarySelectionHandler, PrimarySelectionState, set_primary_focus,
            },
        },
    },
};

use crate::{
    delegate_screencopy,
    protocols::wlr_screencopy::{Screencopy, ScreencopyHandler, ScreencopyManagerState},
    state::State,
};

impl SeatHandler for State {
    type KeyboardFocus = WlSurface;
    type PointerFocus = WlSurface;
    type TouchFocus = WlSurface;

    fn seat_state(&mut self) -> &mut SeatState<Self> {
        &mut self.projectwc.seat_state
    }

    fn focus_changed(&mut self, seat: &Seat<Self>, focused: Option<&WlSurface>) {
        let dh = &self.projectwc.display_handle;
        let client = focused.and_then(|s| dh.get_client(s.id()).ok());
        set_data_device_focus(dh, seat, client.clone());
        set_primary_focus(dh, seat, client);
    }

    fn cursor_image(&mut self, _seat: &Seat<Self>, _image: CursorImageStatus) {}
}

delegate_seat!(State);

impl SelectionHandler for State {
    type SelectionUserData = ();
}

impl DataDeviceHandler for State {
    fn data_device_state(&mut self) -> &mut DataDeviceState {
        &mut self.projectwc.data_device_state
    }
}

impl DndGrabHandler for State {}

impl WaylandDndGrabHandler for State {
    fn dnd_requested<S: smithay::input::dnd::Source>(
        &mut self,
        source: S,
        _icon: Option<WlSurface>,
        seat: Seat<Self>,
        serial: smithay::utils::Serial,
        type_: smithay::input::dnd::GrabType,
    ) {
        match type_ {
            GrabType::Pointer => {
                let ptr = seat.get_pointer().unwrap();
                let start_data = ptr.grab_start_data().unwrap();

                let grab =
                    DnDGrab::new_pointer(&self.projectwc.display_handle, start_data, source, seat);
                ptr.set_grab(self, grab, serial, Focus::Keep);
            }
            // TODO: handle touch grab
            GrabType::Touch => {}
        }
    }
}

delegate_data_device!(State);

impl OutputHandler for State {}

delegate_output!(State);

impl PrimarySelectionHandler for State {
    fn primary_selection_state(&mut self) -> &mut PrimarySelectionState {
        &mut self.projectwc.primary_selection_state
    }
}
delegate_primary_selection!(State);

impl ScreencopyHandler for State {
    fn screencopy_state(&mut self) -> &mut ScreencopyManagerState {
        &mut self.projectwc.screencopy_state
    }

    fn frame(&mut self, screencopy: Screencopy) {
        self.projectwc.pending_screencopy = Some(screencopy);
    }
}

delegate_screencopy!(State);

impl DrmLeaseHandler for State {
    fn drm_lease_state(&mut self, node: DrmNode) -> &mut DrmLeaseState {
        self.backend
            .udev()
            .devices
            .get_mut(&node)
            .unwrap()
            .drm_lease_state
            .as_mut()
            .unwrap()
    }

    fn lease_request(
        &mut self,
        node: DrmNode,
        request: DrmLeaseRequest,
    ) -> Result<DrmLeaseBuilder, LeaseRejected> {
        let device = self.backend.udev().devices.get(&node).unwrap();
        let mut builder = DrmLeaseBuilder::new(&device.drm);
        for connector in request.connectors {
            let (_, crtc) = device
                .non_desktop_connectors
                .iter()
                .find(|(handle, _)| connector == *handle)
                .ok_or(LeaseRejected::default())?;
            builder.add_connector(connector);
            builder.add_crtc(*crtc);

            let planes = device.drm.planes(crtc).map_err(LeaseRejected::with_cause)?;
            let (primary_plane, primary_plane_claim) = planes
                .primary
                .iter()
                .find_map(|plane| {
                    device
                        .drm
                        .claim_plane(plane.handle, *crtc)
                        .map(|claim| (plane, claim))
                })
                .ok_or_else(LeaseRejected::default)?;
            builder.add_plane(primary_plane.handle, primary_plane_claim);
        }

        Ok(builder)
    }

    fn new_active_lease(&mut self, node: DrmNode, lease: smithay::wayland::drm_lease::DrmLease) {
        let device = self.backend.udev().devices.get_mut(&node).unwrap();
        device.active_leases.push(lease);
    }

    fn lease_destroyed(&mut self, node: DrmNode, lease_id: u32) {
        let backend = self.backend.udev().devices.get_mut(&node).unwrap();
        backend.active_leases.retain(|l| l.id() != lease_id);
    }
}

delegate_drm_lease!(State);
