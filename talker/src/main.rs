use clap::{CommandFactory, Parser};

use talker::{cli, core, gui};

#[derive(Parser)]
#[command(
    name = "talker",
    version,
    // Suppress clap's auto `-V` so we can override with lowercase
    // `-v` below. The `version,` line above is still needed so
    // `#[command(version)]` knows what version string to print.
    disable_version_flag = true,
    about = "Send NMEA0183, ASCII, UTF, and binary data over serial and network connections",
    long_about = "Send NMEA0183, ASCII, UTF, and binary data over serial and network \
                  connections. Talker drives one or more send-only channels (serial, \
                  UDP unicast/broadcast/multicast, or TCP client) on a profile-defined \
                  schedule. Run with --gui for the graphical editor, or with \
                  --profile / --profile-path on the command line to start sending \
                  immediately. Profiles are TOML files (see \
                  talker/profiles/profile.example.toml) and live in the \
                  OS-appropriate config directory by default."
)]
struct TopArgs {
    /// Print version information and exit.
    #[arg(
        short = 'v',
        long,
        action = clap::ArgAction::Version,
        help = "Print version information and exit."
    )]
    version: Option<bool>,

    /// Launch the graphical interface instead of the headless CLI runner.
    #[arg(
        short = 'g',
        long,
        help = "Launch the graphical interface instead of the headless CLI runner.",
        long_help = "Launch the graphical interface instead of the headless CLI \
                     runner. With `--gui` and no profile flag, the GUI opens to the \
                     last loaded profile (if any) or an empty editor. `--profile` / \
                     `--profile-path` are honoured and the named profile is loaded on \
                     startup. All other CLI flags (echo, log-file, quiet) only affect \
                     the headless mode."
    )]
    gui: bool,

    #[command(flatten)]
    cli: cli::Args,
}

fn main() {
    // Leading blank line before any output (clap's help / error /
    // version, our own help-on-no-args, or the GUI / CLI's own
    // logs). Visually separates talker from a busy shell prompt.
    // Clap strips leading whitespace from `about` strings, so this
    // can't be done declaratively in the attribute.
    println!();
    let args = TopArgs::parse();

    // No actionable arg → behave like `-h` rather than dropping
    // into `cli::run` and bailing with "no profile specified". The
    // user sees the full option list right away.
    let nothing_to_do = !args.gui
        && args.cli.profile.is_none()
        && args.cli.profile_path.is_none()
        && !args.cli.list_profiles;
    if nothing_to_do {
        let _ = TopArgs::command().print_help();
        println!();
        return;
    }

    if args.gui {
        let initial_profile = if let Some(p) = args.cli.profile_path {
            Some(p)
        } else if let Some(name) = args.cli.profile {
            core::profile::default_dir().map(|d| d.join(format!("{name}.toml")))
        } else {
            None
        };
        if let Err(e) = gui::run(initial_profile) {
            eprintln!("\nerror: {e:#}");
            std::process::exit(1);
        }
        return;
    }

    // The exit code says how the run ended (ADR-060); an internal error is 1.
    match cli::run(args.cli) {
        Ok(0) => {}
        Ok(code) => std::process::exit(i32::from(code)),
        Err(e) => {
            eprintln!("\nerror: {e:#}");
            std::process::exit(1);
        }
    }
}
