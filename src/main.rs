use arpauto_macro::{app, safety};
use std::process::ExitCode;

const HELP: &str = "\
ARPAuto Macro - repeated timed mouse-button holds for Linux X11

Usage: arpauto-macro [OPTIONS]

Options:
  -v, --verbose    Log debug information (same as RUST_LOG=arpauto_macro=debug)
  -h, --help       Show this help
  -V, --version    Show the version

Settings are stored in $XDG_CONFIG_HOME/arpauto-macro/config.toml (default ~/.config).";

fn main() -> ExitCode {
    let mut verbose = false;
    for arg in std::env::args().skip(1) {
        match arg.as_str() {
            "-h" | "--help" => {
                println!("{HELP}");
                return ExitCode::SUCCESS;
            }
            "-V" | "--version" => {
                println!("arpauto-macro {}", env!("CARGO_PKG_VERSION"));
                return ExitCode::SUCCESS;
            }
            "-v" | "--verbose" => verbose = true,
            other => {
                eprintln!("arpauto-macro: unknown option '{other}' (see --help)");
                return ExitCode::from(2);
            }
        }
    }

    // Must happen before any thread is spawned so every thread inherits the mask.
    safety::block_termination_signals();

    let default_filter = if verbose { "arpauto_macro=debug,info" } else { "info" };
    let filter = tracing_subscriber::EnvFilter::try_from_default_env()
        .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(filter).with_target(false).init();

    if std::env::var_os("DISPLAY").is_none() {
        eprintln!("arpauto-macro: no X11 display (DISPLAY is not set). This application requires an X11 session.");
        return ExitCode::FAILURE;
    }

    let _lock = match safety::InstanceLock::acquire("arpauto-macro") {
        Ok(lock) => lock,
        Err(e) => {
            eprintln!("arpauto-macro: {e}");
            return ExitCode::FAILURE;
        }
    };

    match app::run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("arpauto-macro: {e}");
            ExitCode::FAILURE
        }
    }
}
