use projectwc::{CompositorError, Result, state::State};
use smithay::reexports::{calloop::EventLoop, wayland_server::Display};

fn main() -> Result<()> {
    if let Ok(env_filter) = tracing_subscriber::EnvFilter::try_from_default_env() {
        tracing_subscriber::fmt().with_env_filter(env_filter).init();
    } else {
        tracing_subscriber::fmt().init();
    }

    let mut event_loop: EventLoop<State> =
        EventLoop::try_new().map_err(|e| CompositorError::EventLoop(e.to_string()))?;

    let display = Display::new().map_err(|e| CompositorError::Backend(e.to_string()))?;
    let mut state = State::new(event_loop.handle(), event_loop.get_signal(), display).ok_or(
        CompositorError::Backend("Failed to initilize backend".into()),
    )?;

    let spawn_cmd: Option<String> = std::env::args().nth(1);
    if let Some(cmd) = spawn_cmd {
        std::process::Command::new(&cmd).spawn().ok();
    }

    event_loop
        .run(None, &mut state, |_| {})
        .map_err(|e| CompositorError::EventLoop(e.to_string()))?;

    Ok(())
}
