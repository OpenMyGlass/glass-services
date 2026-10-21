use glassd::daemon::{Daemon, Options, TOUCHPAD_NAME};
use glassd::display::LoggingDisplay;
use glassd::touch::ScreenOffInput;
use std::path::PathBuf;
use std::process::ExitCode;

const USAGE: &str = "\
usage: glassd [options]

  --socket <path>         socket path (default: $GLASS_SERVICES_SOCKET, else the platform default)
  --mock                  no hardware; read mock commands on standard input (docs/api.md)
  --device <path>         touchpad event device
  --device-name <name>    find the touchpad by input device name (default: sensor00fn11)
  --invert-x              forward is decreasing raw X
  --invert-y              down is decreasing raw Y
  --screen-off-input <encoded|touches>
                          screen-off wake: the kernel's encoded frames only (Android early
                          suspend), or the recognizer on real touches (default: touches)
  -v, --verbose           log to standard error
  -h, --help              this help";

fn parse(args: impl Iterator<Item = String>) -> Result<(Options, bool), String> {
    let mut o = Options {
        socket: glass_api::socket_path(),
        mock: false,
        device: None,
        device_name: TOUCHPAD_NAME.into(),
        invert_x: false,
        invert_y: false,
        screen_off_input: ScreenOffInput::Touches,
    };
    let mut verbose = false;
    let mut args = args.peekable();
    while let Some(a) = args.next() {
        let mut value = |name: &str| args.next().ok_or(format!("{name} needs a value"));
        match a.as_str() {
            "--socket" => o.socket = PathBuf::from(value("--socket")?),
            "--mock" => o.mock = true,
            "--device" => o.device = Some(PathBuf::from(value("--device")?)),
            "--device-name" => o.device_name = value("--device-name")?,
            "--invert-x" => o.invert_x = true,
            "--invert-y" => o.invert_y = true,
            "--screen-off-input" => {
                o.screen_off_input = match value("--screen-off-input")?.as_str() {
                    "encoded" => ScreenOffInput::Encoded,
                    "touches" => ScreenOffInput::Touches,
                    v => return Err(format!("--screen-off-input: unknown value {v:?}")),
                }
            }
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    Ok((o, verbose))
}

fn main() -> ExitCode {
    let (opts, verbose) = match parse(std::env::args().skip(1)) {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                eprintln!("glassd: {e}");
            }
            eprintln!("{USAGE}");
            return if e.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };
    glassd::log::set_verbose(verbose);
    let mut d = match Daemon::new(&opts, Box::new(LoggingDisplay)) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("glassd: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    match d.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("glassd: {e}");
            ExitCode::FAILURE
        }
    }
}
