//! The single `asd` binary: the terminal-mux CLI + embedded daemon (`local`
//! feature) and the GPU GUI (`gui` feature) combined into one executable.
//!
//! Running `asd` with no subcommand opens the GUI; the terminal commands
//! (`new` / `attach` / `list` / `kill` / `daemon` / `restart`) live under
//! `local`. The features keep the crate boundaries intact — `asd-cli` (which
//! pulls portable-pty via the daemon) and `asd-gui` (iced) stay separate
//! libraries, and only this binary combines them. Build with
//! `--no-default-features --features gui` for a PTY-free, GUI-only client.

fn main() -> anyhow::Result<()> {
    run()
}

/// Full build: the CLI owns the command surface and calls back into the GUI
/// launcher for a no-subcommand / `gui` invocation.
#[cfg(feature = "local")]
fn run() -> anyhow::Result<()> {
    #[cfg(feature = "gui")]
    let gui: Option<asd_cli::GuiLauncher> = Some(launch_gui);
    #[cfg(not(feature = "gui"))]
    let gui: Option<asd_cli::GuiLauncher> = None;
    asd_cli::run(gui)
}

/// GUI-only build (e.g. Windows): no CLI/daemon. Bare `asd`, or
/// `asd [gui] <session>`, opens the window.
#[cfg(all(not(feature = "local"), feature = "gui"))]
fn run() -> anyhow::Result<()> {
    let session = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-') && a != "gui");
    launch_gui(session)
}

#[cfg(not(any(feature = "local", feature = "gui")))]
compile_error!("enable at least one of the `local` or `gui` features");

#[cfg(feature = "gui")]
fn launch_gui(session: Option<String>) -> anyhow::Result<()> {
    asd_gui::run(session).map_err(|e| anyhow::anyhow!("gui error: {e:?}"))
}
