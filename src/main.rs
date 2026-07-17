//! The single `asd` binary: the terminal-mux CLI + embedded daemon (`local`
//! feature) and the GUI combined into one executable.
//!
//! Running `asd` with no subcommand opens the GUI; the terminal commands
//! (`new` / `attach` / `list` / `kill` / `daemon` / `restart`) live under
//! `local`. The GUI (`dioxus` feature: Dioxus Desktop + ghostty-web) stays a
//! separate library, as does `asd-cli` (which pulls portable-pty via the
//! daemon) — only this binary combines them. Build with
//! `--no-default-features --features dioxus` for a PTY-free, GUI-only client.

fn main() -> anyhow::Result<()> {
    run()
}

/// Full build: the CLI owns the command surface and calls back into the GUI
/// launcher for a no-subcommand / `gui` invocation.
#[cfg(feature = "local")]
fn run() -> anyhow::Result<()> {
    #[cfg(feature = "dioxus")]
    let gui: Option<asd_cli::GuiLauncher> = Some(launch_gui);
    #[cfg(not(feature = "dioxus"))]
    let gui: Option<asd_cli::GuiLauncher> = None;
    asd_cli::run(gui)
}

/// GUI-only build (e.g. Windows): no CLI/daemon. Bare `asd`, or
/// `asd [gui] <session>`, opens the window.
#[cfg(all(not(feature = "local"), feature = "dioxus"))]
fn run() -> anyhow::Result<()> {
    let session = std::env::args()
        .skip(1)
        .find(|a| !a.starts_with('-') && a != "gui");
    launch_gui(session)
}

#[cfg(not(any(feature = "local", feature = "dioxus")))]
compile_error!("enable at least one of the `local` or `dioxus` features");

#[cfg(feature = "dioxus")]
fn launch_gui(session: Option<String>) -> anyhow::Result<()> {
    asd_dioxus::run(session)
}
