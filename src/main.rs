//! The single `asd` binary: the terminal-mux CLI + embedded daemon and the GUI
//! combined into one executable.
//!
//! Running `asd` with no subcommand opens the GUI; the terminal commands
//! (`new` / `attach` / `list` / `kill` / `daemon` / `restart`) are always built
//! in. The GUI (`dioxus` feature: Dioxus Desktop + ghostty-web) stays a
//! separate library, as does `asd-cli` (which pulls portable-pty via the
//! daemon) — only this binary combines them. Build with
//! `--no-default-features` for a GUI-free binary that links no WebView.

fn main() {
    if let Err(e) = run() {
        // Same rendering anyhow's own `Termination` gives (message + causes);
        // only the status differs, so a caller can tell "no such session" from
        // "the daemon is down" without matching on wording.
        eprintln!("Error: {e:?}");
        std::process::exit(failure_status(&e));
    }
}

/// Exit status for a failed run. The CLI knows which failures a caller can act
/// on, so it owns the mapping.
fn failure_status(err: &anyhow::Error) -> i32 {
    asd_cli::exit_status(err)
}

/// The CLI owns the command surface and calls back into the GUI launcher for a
/// no-subcommand / `gui` invocation. Without the `dioxus` feature there is no
/// launcher to hand it, and those invocations report that instead.
fn run() -> anyhow::Result<()> {
    #[cfg(feature = "dioxus")]
    let gui: Option<asd_cli::GuiLauncher> = Some(launch_gui);
    #[cfg(not(feature = "dioxus"))]
    let gui: Option<asd_cli::GuiLauncher> = None;
    asd_cli::run(gui)
}

#[cfg(feature = "dioxus")]
fn launch_gui(session: Option<String>) -> anyhow::Result<()> {
    asd_dioxus::run(session)
}
