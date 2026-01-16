use smithay::{
    backend::{
        allocator::{
            Fourcc,
            format::FormatSet,
            gbm::{GbmAllocator, GbmBufferFlags},
        },
        drm::{
            DrmDevice, DrmDeviceFd, DrmEventMetadata, DrmEventTime, DrmNode, GbmBufferedSurface,
            NodeType,
            compositor::{DrmCompositor, FrameFlags},
            exporter::gbm::GbmFramebufferExporter,
        },
        egl::{EGLContext, EGLDevice, EGLDisplay, context::ContextPriority},
        libinput::{LibinputInputBackend, LibinputSessionInterface},
        renderer::{
            self, ImportAll, Renderer,
            element::{
                AsRenderElements,
                solid::{SolidColorBuffer, SolidColorRenderElement},
                surface::WaylandSurfaceRenderElement,
            },
            gles::GlesRenderer,
            multigpu::{GpuManager, gbm::GbmGlesBackend},
        },
        session::{Event as SessionEvent, Session, libseat::LibSeatSession},
        udev::{self, UdevBackend, UdevEvent},
    },
    desktop::{layer_map_for_output, utils::OutputPresentationFeedback},
    output::{Mode as WlMode, Output, OutputModeSource, PhysicalProperties},
    reexports::{
        calloop::{LoopHandle, RegistrationToken, timer::Timer},
        drm::{
            Device as _,
            control::{Device, ModeTypeFlags, connector, crtc},
        },
        gbm::{Device as GbmDevice, Modifier},
        input::Libinput,
        rustix::{fs::OFlags, path::Arg},
        wayland_protocols::wp::{self, presentation_time::server::wp_presentation_feedback},
    },
    render_elements,
    utils::{DeviceFd, Monotonic, Physical, Point, Scale},
    wayland::{
        drm_lease::{DrmLease, DrmLeaseState},
        presentation::Refresh,
        shell::wlr_layer::Layer as WlrLayer,
    },
};
use smithay_drm_extras::{
    display_info,
    drm_scanner::{DrmScanEvent, DrmScanner},
};
use std::{collections::HashMap, fmt::format, path::Path, time::Duration};
use tracing::info;

use crate::{ProjectWC, state::State};

const SUPPORTED_COLOR_FORMATS: [Fourcc; 2] = [Fourcc::Argb8888, Fourcc::Abgr8888];

type GbmDrmCompositor = DrmCompositor<
    GbmAllocator<DrmDeviceFd>,
    GbmFramebufferExporter<DrmDeviceFd>,
    OutputPresentationFeedback,
    DrmDeviceFd,
>;

render_elements! {
    pub OutputRenderElements<R> where R: ImportAll;
    Surface=WaylandSurfaceRenderElement<R>,
    Cursor=SolidColorRenderElement,
}
impl<R: Renderer> std::fmt::Debug for OutputRenderElements<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Surface(e) => f.debug_tuple("Surface").field(e).finish(),
            Self::Cursor(e) => f.debug_tuple("Cursor").field(e).finish(),
            _ => f.write_str("OutputRenderElements"),
        }
    }
}

#[derive(Debug)]
pub struct Udev {
    session: LibSeatSession,
    libinput: Libinput,
    primary_gpu: DrmNode,
    gpu_manager: GpuManager<GbmGlesBackend<GlesRenderer, DrmDeviceFd>>,
    pub devices: HashMap<DrmNode, OutputDevice>,
}

#[derive(Debug)]
pub struct OutputDevice {
    token: RegistrationToken,
    surfaces: HashMap<crtc::Handle, Surface>,
    render_node: Option<DrmNode>,
    allocator: GbmAllocator<DrmDeviceFd>,
    pub drm: DrmDevice,
    drm_scanner: DrmScanner,
    pub drm_lease_state: Option<DrmLeaseState>,
    pub non_desktop_connectors: Vec<(connector::Handle, crtc::Handle)>,
    pub active_leases: Vec<DrmLease>,
    gbm: GbmDevice<DrmDeviceFd>,
}

#[derive(Debug)]
pub struct Surface {
    output_info: OutputInfo,
    compositor: GbmDrmCompositor,
}

#[derive(Debug)]
pub struct OutputInfo {
    connector: String,
    make: String,
    model: String,
    serial: Option<String>,
}

#[derive(Debug)]
pub struct UdevOutputState {
    node: DrmNode,
    crtc: crtc::Handle,
}

impl Udev {
    // TODO: Review error types
    pub fn new(event_loop: LoopHandle<'static, State>, projectwc: &mut ProjectWC) -> Option<Self> {
        let (session, notifier) = LibSeatSession::new().expect("Failed to create session");
        let seat_name = session.seat();
        info!("Created session on seat {seat_name}");

        let udev_backend = UdevBackend::new(&seat_name).expect("Failed to create udev backend");

        let mut libinput = Libinput::new_with_udev(LibinputSessionInterface::from(session.clone()));
        libinput
            .udev_assign_seat(&seat_name)
            .expect("Failed to assign seat to libinput");
        let libinput_backend = LibinputInputBackend::new(libinput.clone());
        event_loop
            .insert_source(libinput_backend, |event, _, state| {
                state.handle_input_event(event);
            })
            .unwrap();
        event_loop
            .insert_source(notifier, |event, (), state| {
                state.backend.udev().on_session_event(event);
            })
            .unwrap();

        let primary_gpu = udev::primary_gpu(&seat_name)
            .ok()?
            .and_then(|gpu_path| {
                DrmNode::from_path(gpu_path)
                    .ok()?
                    .node_with_type(NodeType::Render)?
                    .ok()
            })
            .expect("Failed to get primary GPU");

        info!("Using {primary_gpu} as primary gpu");

        let api = GbmGlesBackend::with_context_priority(ContextPriority::High);
        let gpu_manager = GpuManager::new(api).expect("Failed to create Gpu Manager");

        let mut udev = Self {
            session,
            libinput,
            primary_gpu,
            gpu_manager,
            devices: HashMap::new(),
        };

        for (device_id, path) in udev_backend.device_list() {
            udev.device_added(device_id, path, projectwc).ok()?;
        }

        event_loop
            .insert_source(udev_backend, |event, _, state| {
                state
                    .backend
                    .udev()
                    .on_udev_event(event, &mut state.projectwc);
            })
            .unwrap();

        Some(udev)
    }

    fn on_session_event(&mut self, event: SessionEvent) {
        match event {
            SessionEvent::PauseSession => {
                info!("pausing session");
                self.libinput.suspend();
                for device in self.devices.values_mut() {
                    device.drm.pause();
                    if let Some(lease_state) = &mut device.drm_lease_state {
                        lease_state.suspend();
                    }
                }
            }
            SessionEvent::ActivateSession => {
                info!("activating session");

                if self.libinput.resume().is_err() {
                    tracing::warn!("error resuming libinput");
                }

                for device in self.devices.values_mut() {
                    device.drm.activate(true).unwrap();
                    if let Some(lease_sate) = &mut device.drm_lease_state {
                        // TODO: resume drm lease
                        lease_sate.resume::<State>();
                    }
                }
            }
        }
    }

    fn on_udev_event(&mut self, event: UdevEvent, projectwc: &mut ProjectWC) {
        match event {
            UdevEvent::Added { device_id, path } => {
                if !self.session.is_active() {
                    return;
                }
                // TODO: Log errors
                let _ = self.device_added(device_id, &path, projectwc);
            }
            UdevEvent::Changed { device_id } => {
                if !self.session.is_active() {
                    return;
                }

                let node = DrmNode::from_dev_id(device_id).unwrap();
                self.device_changed(node, projectwc);
            }
            UdevEvent::Removed { device_id } => {
                if !self.session.is_active() {
                    return;
                }
                let node = DrmNode::from_dev_id(device_id).unwrap();
                self.device_removed(node, projectwc);
            }
        }
    }

    // TODO: Review error type
    fn device_added(
        &mut self,
        device_id: u64,
        path: &Path,
        projectwc: &mut ProjectWC,
    ) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
        let node = DrmNode::from_dev_id(device_id)?;
        let fd = self.session.open(path, OFlags::RDWR | OFlags::CLOEXEC)?;
        let device_fd = DrmDeviceFd::new(DeviceFd::from(fd));

        let (drm, drm_notifier) = DrmDevice::new(device_fd.clone(), false)?;
        let gbm = GbmDevice::new(device_fd)?;

        let token = projectwc
            .loop_handle
            .insert_source(drm_notifier, move |event, metadata, state| match event {
                smithay::backend::drm::DrmEvent::VBlank(crtc) => {
                    let metadata = metadata.expect("vblank events must have metadata");
                    state
                        .backend
                        .udev()
                        .on_vblank(&mut state.projectwc, node, crtc, metadata);
                }
                smithay::backend::drm::DrmEvent::Error(err) => {
                    tracing::warn!("DRM error: {:?}", err)
                }
            })
            .unwrap();

        let mut try_initialize_gpu = || {
            let display = unsafe { EGLDisplay::new(gbm.clone()).unwrap() };
            let egl_device = EGLDevice::device_for_display(&display).unwrap();

            if egl_device.is_software() {
                return Err("No render node found");
            }

            let render_node = egl_device
                .try_get_render_node()
                .ok()
                .flatten()
                .unwrap_or(node);
            self.gpu_manager
                .as_mut()
                .add_node(render_node, gbm.clone())
                .unwrap();

            Ok(render_node)
        };

        let render_node = try_initialize_gpu()
            .inspect_err(|err| tracing::warn!("failed to initialize gpu: {err:?}"))
            .ok();

        let allocator_gbm = if render_node.is_some() {
            gbm.clone()
        } else if let Some(primary_device) = self.devices.get(&self.primary_gpu) {
            primary_device.gbm.clone()
        } else {
            return Err("no allocator for device".into());
        };
        let allocator = GbmAllocator::new(
            allocator_gbm,
            GbmBufferFlags::RENDERING | GbmBufferFlags::SCANOUT,
        );

        let drm_lease_state = DrmLeaseState::new::<State>(&projectwc.display_handle, &node).ok();

        let device = OutputDevice {
            token,
            gbm,
            drm,
            drm_lease_state,
            render_node,
            allocator,
            drm_scanner: DrmScanner::new(),
            non_desktop_connectors: Vec::new(),
            surfaces: HashMap::new(),
            active_leases: Vec::new(),
        };

        self.devices.insert(node, device);
        self.device_changed(node, projectwc);

        Ok(())
    }

    fn device_changed(&mut self, node: DrmNode, projectwc: &mut ProjectWC) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let Ok(scan_result) = device.drm_scanner.scan_connectors(&device.drm) else {
            tracing::warn!("Failed to scan connectors");
            return;
        };

        for event in scan_result {
            match event {
                DrmScanEvent::Connected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_connected(node, connector, crtc, projectwc);
                }
                DrmScanEvent::Disconnected {
                    connector,
                    crtc: Some(crtc),
                } => {
                    self.connector_disconnected(node, connector, crtc, projectwc);
                }
                _ => {}
            }
        }
    }

    fn device_removed(&mut self, node: DrmNode, projectwc: &mut ProjectWC) {
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let crtcs: Vec<_> = device
            .drm_scanner
            .crtcs()
            .map(|(info, crtc)| (info.clone(), crtc))
            .collect();

        for (conn, crtc) in crtcs {
            self.connector_disconnected(node, conn, crtc, projectwc);
        }

        if let Some(mut device) = self.devices.remove(&node) {
            if let Some(lease_state) = device.drm_lease_state.as_mut() {
                lease_state.disable_global::<State>();
            }

            if let Some(render_node) = device.render_node {
                self.gpu_manager.as_mut().remove_node(&render_node);
            }

            projectwc.loop_handle.remove(device.token);
            tracing::debug!("Dropping render device");
        }
    }

    fn connector_connected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
        projectwc: &mut ProjectWC,
    ) {
        let connector_name = format!(
            "{}-{}",
            connector.interface().as_str(),
            connector.interface_id(),
        );
        tracing::info!("connecting connector: {connector_name}");

        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };

        let non_desktop = device
            .drm
            .get_properties(connector.handle())
            .ok()
            .and_then(|props| {
                let (info, value) = props
                    .into_iter()
                    .filter_map(|(handle, value)| {
                        let info = device.drm.get_property(handle).ok()?;
                        Some((info, value))
                    })
                    .find(|(info, _)| info.name().to_str() == Ok("non-desktop"))?;

                info.value_type().convert_value(value).as_boolean()
            })
            .unwrap_or(false);

        let display_info = display_info::for_connector(&device.drm, connector.handle());

        let make = display_info
            .as_ref()
            .and_then(|info| info.make())
            .unwrap_or_else(|| "Unknown".into());

        let model = display_info
            .as_ref()
            .and_then(|info| info.model())
            .unwrap_or_else(|| "Unkown".into());

        let serial_number = display_info
            .as_ref()
            .and_then(|info| info.serial())
            .unwrap_or_else(|| "Unkown".into());

        let output_info = OutputInfo {
            make: make.clone(),
            model: model.clone(),
            connector: connector_name.clone(),
            serial: Some(serial_number.clone()),
        };

        if non_desktop {
            tracing::debug!("Connector {} is non desktop", connector_name);
            device
                .non_desktop_connectors
                .push((connector.handle(), crtc));

            if let Some(lease_state) = &mut device.drm_lease_state {
                lease_state.add_connector::<State>(
                    connector.handle(),
                    connector_name,
                    format!("{make} {model}"),
                );
            }
        } else {
            let mode_id = connector
                .modes()
                .iter()
                .position(|mode| mode.mode_type().contains(ModeTypeFlags::PREFERRED))
                .unwrap_or(0);

            let drm_mode = connector.modes()[mode_id];
            let wl_mode = WlMode::from(drm_mode);

            let surface = device
                .drm
                .create_surface(crtc, drm_mode, &[connector.handle()])
                .unwrap();

            let (phy_w, phy_h) = connector.size().unwrap_or((0, 0));
            let output = Output::new(
                connector_name,
                PhysicalProperties {
                    size: (phy_w as i32, phy_h as i32).into(),
                    subpixel: connector.subpixel().into(),
                    make,
                    model,
                    serial_number,
                },
            );

            let global = output.create_global::<State>(&projectwc.display_handle);
            let x = projectwc.space.outputs().fold(0, |acc, o| {
                acc + projectwc.space.output_geometry(o).unwrap().size.w
            });
            let position = (x, 0).into();

            output.set_preferred(wl_mode);
            output.change_current_state(Some(wl_mode), None, None, Some(position));
            projectwc.space.map_output(&output, position);

            output
                .user_data()
                .insert_if_missing(|| UdevOutputState { node, crtc });

            let mut planes = device.drm.planes(&crtc).unwrap();
            // overlay planes bad, idk
            planes.overlay.clear();

            let renderer = self.gpu_manager.single_renderer(&self.primary_gpu).unwrap();
            let egl_context = renderer.as_ref().egl_context();
            let render_formats = egl_context.dmabuf_render_formats();

            let render_formats = render_formats
                .iter()
                .copied()
                .filter(|format| {
                    !matches!(
                        format.modifier,
                        Modifier::I915_y_tiled_ccs
                        | Modifier::Unrecognized(0x100000000000005)
                        | Modifier::I915_y_tiled_gen12_mc_ccs
                        | Modifier::I915_y_tiled_gen12_rc_ccs
                        // I915_FORMAT_MOD_Y_TILED_GEN12_RC_CCS_CC
                        | Modifier::Unrecognized(0x100000000000008)
                        // I915_FORMAT_MOD_4_TILED_DG2_RC_CCS
                        | Modifier::Unrecognized(0x10000000000000a)
                        // I915_FORMAT_MOD_4_TILED_DG2_MC_CCS
                        | Modifier::Unrecognized(0x10000000000000b)
                        // I915_FORMAT_MOD_4_TILED_DG2_RC_CCS_CC
                        | Modifier::Unrecognized(0x10000000000000c)
                    )
                })
                .collect::<FormatSet>();

            let res = DrmCompositor::new(
                OutputModeSource::Auto(output.clone()),
                surface,
                Some(planes.clone()),
                device.allocator.clone(),
                GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
                SUPPORTED_COLOR_FORMATS,
                render_formats.clone(),
                device.drm.cursor_size(),
                Some(device.gbm.clone()),
            );

            let compositor = res.unwrap_or_else(|err| {
                tracing::warn!(
                    "failed to create DRM compositor, falling back to render_formats with invalid modifier: {err:?}"
                );
                let render_formats = render_formats
                    .iter()
                    .copied()
                    .filter(|format| format.modifier == Modifier::Invalid)
                    .collect::<FormatSet>();

                // DrmCompositor::new() consumed the surface...
                let surface = device
                    .drm
                    .create_surface(crtc, drm_mode, &[connector.handle()])
                    .unwrap();

                DrmCompositor::new(
                    OutputModeSource::Auto(output.clone()),
                    surface,
                    Some(planes),
                    device.allocator.clone(),
                    GbmFramebufferExporter::new(device.gbm.clone(), device.render_node.into()),
                    SUPPORTED_COLOR_FORMATS,
                    render_formats,
                    device.drm.cursor_size(),
                    Some(device.gbm.clone()),
                )
                .expect("error creating DRM compositor")
            });

            let surface = Surface {
                output_info,
                compositor,
            };

            device.surfaces.insert(crtc, surface);

            self.render(projectwc, node, &output);
        }
    }

    fn connector_disconnected(
        &mut self,
        node: DrmNode,
        connector: connector::Info,
        crtc: crtc::Handle,
        projectwc: &mut ProjectWC,
    ) {
        tracing::debug!("disconnecting connector: {connector}");

        let Some(device) = self.devices.get_mut(&node) else {
            tracing::debug!("missing device");
            return;
        };

        if device.surfaces.remove(&crtc).is_none() {
            if let Some(pos) = device
                .non_desktop_connectors
                .iter()
                .position(|(_, crtc_)| *crtc_ == crtc)
            {
                device.non_desktop_connectors.remove(pos);

                if let Some(lease_state) = device.drm_lease_state.as_mut() {
                    lease_state.withdraw_connector(connector.handle());
                }
            } else {
                tracing::debug!("crtc wasn't enabled");
            }
        }

        let output = projectwc
            .space
            .outputs()
            .find(|output| {
                let udev_state = output.user_data().get::<UdevOutputState>().unwrap();
                udev_state.node == node && udev_state.crtc == crtc
            })
            .cloned();

        if let Some(output) = output {
            projectwc.space.unmap_output(&output);
            projectwc.space.refresh();
        }
    }

    fn on_vblank(
        &mut self,
        projectwc: &mut ProjectWC,
        node: DrmNode,
        crtc: crtc::Handle,
        meta: DrmEventMetadata,
    ) {
        let Some(device) = self.devices.get_mut(&node) else {
            tracing::error!("no device for vblank callback");
            return;
        };

        let Some(surface) = device.surfaces.get_mut(&crtc) else {
            tracing::error!("no crtc for vblank callback: {:?}", crtc);
            return;
        };

        let presentation_time = match meta.time {
            DrmEventTime::Monotonic(time) => time,
            DrmEventTime::Realtime(_) => Duration::ZERO,
        };

        match surface.compositor.frame_submitted() {
            Ok(Some(mut feedback)) => {
                let seq = meta.sequence as u64;
                let flags = wp_presentation_feedback::Kind::Vsync
                    | wp_presentation_feedback::Kind::HwClock
                    | wp_presentation_feedback::Kind::HwCompletion;

                let output = feedback.output().unwrap();
                let refresh = output
                    .current_mode()
                    .map(|mode| Duration::from_secs_f64(1_000f64 / mode.refresh as f64))
                    .map(Refresh::Fixed)
                    .unwrap_or(Refresh::Unknown);

                feedback.presented::<_, Monotonic>(presentation_time, refresh, seq, flags);
            }
            Ok(None) => {}
            Err(err) => tracing::error!("error marking frame as submitted {}", err),
        }

        let output = projectwc
            .space
            .outputs()
            .find(|output| {
                let udev_state = output.user_data().get::<UdevOutputState>().unwrap();
                udev_state.node == node && udev_state.crtc == crtc
            })
            .cloned()
            .unwrap();

        projectwc
            .loop_handle
            .insert_source(
                Timer::from_duration(presentation_time),
                move |_, _, state| {
                    tracing::debug!("Rendering on vblank");
                    state
                        .backend
                        .udev()
                        .render(&mut state.projectwc, node, &output);
                    smithay::reexports::calloop::timer::TimeoutAction::Drop
                },
            )
            .unwrap();
    }

    fn render(&mut self, projectwc: &mut ProjectWC, node: DrmNode, output: &Output) {
        tracing::debug!("Udev::render called");
        let Some(device) = self.devices.get_mut(&node) else {
            return;
        };
        let udev_state = output.user_data().get::<UdevOutputState>().unwrap();
        let Some(surface) = device.surfaces.get_mut(&udev_state.crtc) else {
            return;
        };
        tracing::trace!(?surface.output_info, "acquired surface");
        let cursor_size = 16;
        let cursor_buffer = SolidColorBuffer::new((cursor_size, cursor_size), [1.0, 1.0, 1.0, 1.0]);
        let cursor_pos: Point<i32, Physical> =
            projectwc.pointer_location.to_physical_precise_round(1);
        let cursor_element = SolidColorRenderElement::from_buffer(
            &cursor_buffer,
            cursor_pos,
            Scale::from(1.0),
            1.0,
            renderer::element::Kind::Cursor,
        );

        let layer_map = layer_map_for_output(&output);
        let upper = layer_map
            .layers()
            .filter(|surface| matches!(surface.layer(), WlrLayer::Top | WlrLayer::Overlay))
            .filter_map(|surface| {
                let geometry = layer_map.layer_geometry(surface)?;
                Some((surface, geometry.loc.to_physical_precise_round(1.0)))
            });

        let lower = layer_map
            .layers()
            .filter(|surface| matches!(surface.layer(), WlrLayer::Background | WlrLayer::Bottom))
            .filter_map(|surface| {
                let geometry = layer_map.layer_geometry(surface)?;
                Some((surface, geometry.loc.to_physical_precise_round(1.0)))
            });

        let mut custom_elements = vec![OutputRenderElements::Cursor(cursor_element)];

        let mut renderer = self.gpu_manager.single_renderer(&self.primary_gpu).unwrap();
        let drm_commpositor = &mut surface.compositor;

        custom_elements.extend(upper.flat_map(|(surface, location)| {
            surface.render_elements(&mut renderer, location, 1.0.into(), 1.0)
        }));

        custom_elements.extend(lower.flat_map(|(surface, location)| {
            surface.render_elements(&mut renderer, location, 1.0.into(), 1.0)
        }));

        match drm_commpositor.render_frame(
            &mut renderer,
            &custom_elements,
            [0.1, 0.1, 0.1, 1.0],
            FrameFlags::DEFAULT,
        ) {
            Ok(res) => {
                if res.is_empty {
                    return;
                }
            }
            Err(err) => {
                drm_commpositor.reset_buffers();
                tracing::error!("error rendering frame {:?}", err);
            }
        }
        projectwc.space.elements().for_each(|window| {
            window.send_frame(
                output,
                projectwc.start_time.elapsed(),
                Some(Duration::ZERO),
                |_, _| Some(output.clone()),
            );
        });
        projectwc.space.refresh();
        projectwc.display_handle.flush_clients().ok();
    }
}
