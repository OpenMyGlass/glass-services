use glassd::daemon::{Daemon, Options, Source, TOUCHPAD_NAME};
use glassd::display::{Display, FbDisplay, LoggingDisplay, TOUCHPAD_SYSFS, default_fb};
use glassd::peers::{PACKAGES_LIST, PeerPolicy};
use glassd::settings_store::default_state_dir;
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
                          suspend), or the recognizer on real touches (default: encoded on
                          Android, touches elsewhere)
  --display <fb[:path]|log>
                          screen back end: FBIOBLANK on the framebuffer (default
                          /dev/graphics/fb0 on Android, /dev/fb0 elsewhere), or log only
                          (default: fb on Linux; log on Android, where the framework
                          blanks the panel, and with --mock)
  --glasshub <dir|none>   glasshub sysfs directory (default /sys/bus/i2c/devices/4-0035)
  --gaze-params <path>    take the glasshub thresholds from this stock parameter file
                          (default /data/system/gaze/gaze_params.txt, if it exists)
  --camera-device <path|none>
                          camera button event device (default: found by name, gpio-keys)
  --power-device <path|none>
                          power button event device (default: found by name, twl6030_pwrbutton)
  --touchpad-sysfs <dir|none>
                          touchpad F11 sysfs directory for sleepmode
                          (default /sys/bus/rmi/devices/touchpad.fn11)
  --head <on|off>         read the IMU for head gestures (default: on on Linux; off on
                          Android, where the sensors HAL owns the IMU)
  --iio <dir|none>        where to look for the IMU (IIO device named mpu9150; default
                          /sys/bus/iio/devices)
  --iio-device <path>     the IMU's character device (default /dev/iio:deviceN)
  --peer-check <on|off>   check each peer's uid (default: on on Android, where the socket
                          is 0666; off elsewhere unless --allow-* is given). Root and
                          system (1000) are always allowed
  --allow-package <name>  allow this Android package's uid (from --packages-list); repeatable
  --allow-uid <uid>       allow this uid; repeatable
  --packages-list <path>  Android's package list (default /data/system/packages.list)
  --state-dir <dir>       where the settings are stored (default: $GLASSD_STATE_DIR, else
                          /data/glassd on Android and /var/lib/glassd elsewhere; with
                          --mock, only when given)
  -v, --verbose           log to standard error
  -h, --help              this help";

#[derive(Debug, Clone, PartialEq, Eq)]
enum DisplayChoice {
    Default,
    Log,
    Fb(Option<PathBuf>),
}

fn path_or_none(v: String) -> Option<PathBuf> {
    (v != "none").then(|| PathBuf::from(v))
}

fn source(v: String) -> Source {
    match path_or_none(v) {
        Some(p) => Source::Path(p),
        None => Source::Off,
    }
}

struct Parsed {
    opts: Options,
    verbose: bool,
    display: DisplayChoice,
}

fn parse(args: impl Iterator<Item = String>) -> Result<Parsed, String> {
    let mut o = Options {
        socket: glass_api::socket_path(),
        mock: false,
        device: None,
        device_name: TOUCHPAD_NAME.into(),
        invert_x: false,
        invert_y: false,
        // Android builds run with early suspend, where the kernel sends encoded frames.
        screen_off_input: if cfg!(target_os = "android") {
            ScreenOffInput::Encoded
        } else {
            ScreenOffInput::Touches
        },
        glasshub: Some(glass_hub::sysfs::DEFAULT_ROOT.into()),
        gaze_params: Some(glass_hub::sysfs::STOCK_PARAMS.into()),
        camera: Source::Auto,
        power: Source::Auto,
        touchpad_sysfs: Some(TOUCHPAD_SYSFS.into()),
        state_dir: None,
        iio: Some(glass_head::iio::DEVICES_ROOT.into()),
        iio_dev: None,
        peers: PeerPolicy::open(),
    };
    let mut peer_check = None;
    let mut allow_uids = Vec::new();
    let mut allow_packages = Vec::new();
    let mut packages_list = PathBuf::from(PACKAGES_LIST);
    let mut verbose = false;
    let mut display = DisplayChoice::Default;
    let mut state_dir = None;
    // On Android (route 1) the IMU belongs to the sensors HAL; head gestures come through
    // the glue there (milestone 3) unless --head on gives the IMU to glassd.
    let mut head = !cfg!(target_os = "android");
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
            "--display" => {
                display = match value("--display")?.as_str() {
                    "log" => DisplayChoice::Log,
                    "fb" => DisplayChoice::Fb(None),
                    v => match v.strip_prefix("fb:") {
                        Some(p) if !p.is_empty() => DisplayChoice::Fb(Some(p.into())),
                        _ => return Err(format!("--display: unknown value {v:?}")),
                    },
                }
            }
            "--glasshub" => o.glasshub = path_or_none(value("--glasshub")?),
            "--gaze-params" => o.gaze_params = path_or_none(value("--gaze-params")?),
            "--camera-device" => o.camera = source(value("--camera-device")?),
            "--power-device" => o.power = source(value("--power-device")?),
            "--touchpad-sysfs" => o.touchpad_sysfs = path_or_none(value("--touchpad-sysfs")?),
            "--state-dir" => state_dir = Some(PathBuf::from(value("--state-dir")?)),
            "--iio" => o.iio = path_or_none(value("--iio")?),
            "--head" => {
                head = match value("--head")?.as_str() {
                    "on" => true,
                    "off" => false,
                    v => return Err(format!("--head: unknown value {v:?}")),
                }
            }
            "--iio-device" => o.iio_dev = Some(PathBuf::from(value("--iio-device")?)),
            "--peer-check" => {
                peer_check = match value("--peer-check")?.as_str() {
                    "on" => Some(true),
                    "off" => Some(false),
                    v => return Err(format!("--peer-check: unknown value {v:?}")),
                }
            }
            "--allow-package" => allow_packages.push(value("--allow-package")?),
            "--allow-uid" => {
                let v = value("--allow-uid")?;
                allow_uids.push(
                    v.parse()
                        .map_err(|_| format!("--allow-uid: not a uid: {v:?}"))?,
                );
            }
            "--packages-list" => packages_list = PathBuf::from(value("--packages-list")?),
            "-v" | "--verbose" => verbose = true,
            "-h" | "--help" => return Err(String::new()),
            other => return Err(format!("unknown option {other:?}")),
        }
    }
    o.state_dir = state_dir.or_else(|| (!o.mock).then(default_state_dir));
    let check = peer_check.unwrap_or(
        cfg!(target_os = "android") || !allow_uids.is_empty() || !allow_packages.is_empty(),
    );
    o.peers = PeerPolicy::new(check, allow_uids, allow_packages, &packages_list);
    if !head {
        o.iio = None;
    }
    Ok(Parsed {
        opts: o,
        verbose,
        display,
    })
}

fn display(choice: DisplayChoice, mock: bool) -> Box<dyn Display> {
    let path = match choice {
        DisplayChoice::Log => return Box::new(LoggingDisplay),
        // On Android (route 1) the framework owns panel blanking (PowerManager, SurfaceFlinger,
        // hwcomposer); the glue turns ScreenState into PowerManager calls (milestone 3).
        DisplayChoice::Default if mock || cfg!(target_os = "android") => {
            return Box::new(LoggingDisplay);
        }
        DisplayChoice::Default | DisplayChoice::Fb(None) => default_fb(),
        DisplayChoice::Fb(Some(p)) => p,
    };
    match FbDisplay::open(&path) {
        Ok(d) => Box::new(d),
        Err(e) => {
            glassd::log::write(format_args!(
                "glassd: warning: cannot open {} ({e}); the screen is not blanked",
                path.display()
            ));
            Box::new(LoggingDisplay)
        }
    }
}

fn main() -> ExitCode {
    let parsed = match parse(std::env::args().skip(1)) {
        Ok(v) => v,
        Err(e) => {
            if !e.is_empty() {
                glassd::log::write(format_args!("glassd: {e}"));
            }
            glassd::log::write(format_args!("{USAGE}"));
            return if e.is_empty() {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(2)
            };
        }
    };
    let Parsed {
        opts,
        verbose,
        display: choice,
    } = parsed;
    glassd::log::set_verbose(verbose);
    let mut d = match Daemon::new(&opts, display(choice, opts.mock)) {
        Ok(d) => d,
        Err(e) => {
            glassd::log::write(format_args!("glassd: cannot start: {e}"));
            return ExitCode::FAILURE;
        }
    };
    match d.run() {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            glassd::log::write(format_args!("glassd: {e}"));
            ExitCode::FAILURE
        }
    }
}
