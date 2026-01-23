use std::time::Duration;

use smithay::{
    backend::{
        allocator::Fourcc,
        renderer::{
            ExportMem,
            damage::OutputDamageTracker,
            element::surface::WaylandSurfaceRenderElement,
            gles::{GlesRenderer, GlesTarget},
        },
        winit::{self, WinitEvent, WinitGraphicsBackend},
    },
    output::{Mode, Output, PhysicalProperties, Subpixel},
    reexports::wayland_server::protocol::wl_shm::Format,
    utils::{Physical, Rectangle, Size, Transform},
    wayland::shm,
};

use crate::{
    CompositorError, ProjectWC, Result, protocols::wlr_screencopy::Screencopy, state::State,
};

pub struct Winit {
    output: Output,
    backend: WinitGraphicsBackend<GlesRenderer>,
    damage_tracker: OutputDamageTracker,
}

impl Winit {
    pub fn new(projectwc: &mut ProjectWC) -> Result<Self> {
        let (backend, winit) = winit::init::<GlesRenderer>()
            .map_err(|e| CompositorError::Backend(format!("{:?}", e)))?;

        let physical_properties = PhysicalProperties {
            size: (0, 0).into(),
            subpixel: Subpixel::Unknown,
            make: "projectwc".into(),
            model: "winit".into(),
            serial_number: "Unknown".into(),
        };

        let mode = Mode {
            size: backend.window_size(),
            refresh: 60_000,
        };

        let output = Output::new("winit".into(), physical_properties);
        output.create_global::<State>(&projectwc.display_handle);
        output.change_current_state(Some(mode), Some(Transform::Flipped180), None, None);
        output.set_preferred(mode);

        projectwc.space.map_output(&output, (0, 0));

        let damage_tracker = OutputDamageTracker::from_output(&output);

        // Set WAYLAND_DISPLAY for child processes
        unsafe { std::env::set_var("WAYLAND_DISPLAY", &projectwc.socket_name) };

        projectwc
            .loop_handle
            .insert_source(winit, move |event, _, state| match event {
                WinitEvent::Resized { size, .. } => {
                    let winit = state.backend.winit();
                    winit.output.change_current_state(
                        Some(smithay::output::Mode {
                            size,
                            refresh: 60_000,
                        }),
                        None,
                        None,
                        None,
                    );
                    state.projectwc.apply_layout().ok();
                }
                WinitEvent::Input(event) => state.handle_input_event(event),
                WinitEvent::Redraw => {
                    let winit = state.backend.winit();
                    let backend = &mut winit.backend;

                    let size = backend.window_size();
                    let damage = Rectangle::from_size(size);

                    let pending_screencopy = state.projectwc.pending_screencopy.take();

                    {
                        let (renderer, mut framebuffer) =
                            backend.bind().expect("failed to bind winit window");
                        smithay::desktop::space::render_output::<
                            _,
                            WaylandSurfaceRenderElement<GlesRenderer>,
                            _,
                            _,
                        >(
                            &winit.output,
                            renderer,
                            &mut framebuffer,
                            1.0,
                            0,
                            [&state.projectwc.space],
                            &[],
                            &mut winit.damage_tracker,
                            make_rgb(150., 154., 171., 1.0),
                        )
                        .unwrap();
                    }

                    backend
                        .submit(Some(&[damage]))
                        .expect("failed to submit damage");

                    if let Some(screencopy) = pending_screencopy
                        && screencopy.output() == &winit.output
                    {
                        let (renderer, framebuffer) =
                            backend.bind().expect("failed to bind for screencopy");
                        if let Err(err) = render_screencopy(
                            renderer,
                            &framebuffer,
                            &winit.output,
                            screencopy,
                            state.projectwc.start_time,
                        ) {
                            tracing::warn!("screencopy failed: {err:?}");
                        }
                    }

                    state.projectwc.space.elements().for_each(|window| {
                        window.send_frame(
                            &winit.output,
                            state.projectwc.start_time.elapsed(),
                            Some(Duration::ZERO),
                            |_, _| Some(winit.output.clone()),
                        );
                    });

                    state.projectwc.space.refresh();
                    state.projectwc.display_handle.flush_clients().unwrap();

                    // Ask for redraw to schedule new frame.
                    backend.window().request_redraw();
                }
                WinitEvent::CloseRequested => state.projectwc.loop_signal.stop(),
                _ => (),
            })
            .map_err(|e| CompositorError::Backend(format!("{:?}", e)))?;

        let winit = Self {
            output,
            backend,
            damage_tracker,
        };

        Ok(winit)
    }
}

fn render_screencopy(
    renderer: &mut GlesRenderer,
    target: &GlesTarget<'_>,
    _output: &Output,
    screencopy: Screencopy,
    start_time: std::time::Instant,
) -> Result<()> {
    let size = screencopy.buffer_size();
    let buffer_size = Size::<i32, Physical>::from((size.w, size.h))
        .to_logical(1)
        .to_buffer(1, Transform::Normal);
    let rect = Rectangle::from_size(buffer_size);

    let mapping = renderer
        .copy_framebuffer(target, rect, Fourcc::Xrgb8888)
        .map_err(|e| CompositorError::Screencopy(format!("copy_framebuffer: {e:?}")))?;
    let bytes = renderer
        .map_texture(&mapping)
        .map_err(|e| CompositorError::Screencopy(format!("map_texture: {e:?}")))?;

    shm::with_buffer_contents_mut(&screencopy.buffer, |shm_buffer, shm_len, buffer_data| {
        if buffer_data.format != Format::Xrgb8888
            || buffer_data.width != size.w
            || buffer_data.height != size.h
            || buffer_data.stride != size.w * 4
            || shm_len != buffer_data.stride as usize * buffer_data.height as usize
        {
            tracing::warn!(
                "buffer validation failed: format={:?} size={}x{} stride={} len={}",
                buffer_data.format,
                buffer_data.width,
                buffer_data.height,
                buffer_data.stride,
                shm_len
            );
            return;
        }
        let dst = unsafe { std::slice::from_raw_parts_mut(shm_buffer.cast::<u8>(), shm_len) };
        dst.copy_from_slice(&bytes[..shm_len]);
    })
    .map_err(|e| CompositorError::Screencopy(format!("shm buffer: {e:?}")))?;

    screencopy.submit(start_time.elapsed());

    Ok(())
}

fn make_rgb(r: f32, g: f32, b: f32, a: f32) -> [f32; 4] {
    [r / 255.0, g / 255.0, b / 255.0, a]
}
