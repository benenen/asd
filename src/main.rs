//! The single `asd` binary: the terminal-mux CLI + embedded daemon (`local`
//! feature) and a GPU GUI combined into one executable.
//!
//! Running `asd` with no subcommand opens the GUI; the terminal commands
//! (`new` / `attach` / `list` / `kill` / `daemon` / `restart`) live under
//! `local`. Two GUI flavors exist as features: `dioxus` (webview +
//! ghostty-web, the default) and `iced` (wgpu-rendered). The features keep
//! the crate boundaries intact — `asd-cli` (which pulls portable-pty via the
//! daemon) and the GUI crates stay separate libraries, and only this binary
//! combines them. Build with `--no-default-features --features dioxus` for a
//! PTY-free, GUI-only client.

fn main() -> anyhow::Result<()> {
    run()
}

/// Full build: the CLI owns the command surface and calls back into the GUI
/// launcher for a no-subcommand / `gui` invocation.
#[cfg(feature = "local")]
fn run() -> anyhow::Result<()> {
    #[cfg(any(feature = "iced", feature = "dioxus"))]
    let gui: Option<asd_cli::GuiLauncher> = Some(launch_gui);
    #[cfg(not(any(feature = "iced", feature = "dioxus")))]
    let gui: Option<asd_cli::GuiLauncher> = None;
    asd_cli::run(gui)
}

/// GUI-only build (e.g. Windows): no CLI/daemon. Bare `asd`, or
/// `asd [gui] <session>`, opens the window.
#[cfg(all(not(feature = "local"), any(feature = "iced", feature = "dioxus")))]
fn run() -> anyhow::Result<()> {
    let session = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-') && a != "gui");
    launch_gui(session)
}

#[cfg(not(any(feature = "local", feature = "iced", feature = "dioxus")))]
compile_error!("enable at least one of the `local`, `dioxus`, or `iced` features");

/// `iced` wins when both GUI features are enabled: the default GUI is dioxus,
/// so iced being on at all means it was requested explicitly
/// (`--features iced` adds to the default set without disabling it).
#[cfg(feature = "iced")]
fn launch_gui(session: Option<String>) -> anyhow::Result<()> {
    asd_gui::run(session).map_err(|e| anyhow::anyhow!("gui error: {e:?}"))
}

#[cfg(all(feature = "dioxus", not(feature = "iced")))]
fn launch_gui(session: Option<String>) -> anyhow::Result<()> {
    asd_dioxus::run(session)
}
