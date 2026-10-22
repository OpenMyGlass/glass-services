//! Commands of `glassd --mock` on standard input (docs/api.md "Mock").

use glass_gesture::synth::Direction;

#[derive(Debug, Clone, PartialEq)]
pub enum Command {
    Tap {
        fingers: u32,
    },
    LongPress,
    Swipe {
        dir: Direction,
        fingers: u32,
    },
    /// A slow drag of this many pad widths (positive forward), no swipe.
    Scroll {
        displacement: f32,
    },
    Camera {
        long: bool,
    },
    Power {
        long: bool,
    },
    Hinge {
        open: bool,
    },
    LookUp,
    LookDown,
    RelativeLookUp,
    Nudge,
    Wear {
        on: bool,
    },
    Wink,
    Dim,
    Wait {
        ms: u64,
    },
    /// The simulated companion link provider (milestone 3).
    Companion(CompanionCommand),
}

/// Commands of the simulated companion link provider.
#[derive(Debug, Clone, PartialEq)]
pub enum CompanionCommand {
    /// Register (if needed) with a connected phone offering these capabilities.
    On { capabilities: Vec<String> },
    /// The phone disconnects; the provider stays registered.
    Off,
    /// The provider unregisters (disconnects from glassd).
    Gone,
    /// A transcript for the newest open request.
    Transcript { text: String, last: bool },
    /// The newest open request fails with this error.
    Error { error: String },
    /// The phone's `ReplyResult` for the newest open request.
    ReplyResult { ok: bool },
}

fn fingers(arg: Option<&str>) -> Result<u32, String> {
    match arg {
        None => Ok(1),
        Some(a) => match a.parse::<u32>() {
            Ok(n @ 1..=3) => Ok(n),
            _ => Err(format!("fingers must be 1, 2 or 3, not {a:?}")),
        },
    }
}

fn long(arg: Option<&str>) -> Result<bool, String> {
    match arg {
        None => Ok(false),
        Some("long") => Ok(true),
        Some(a) => Err(format!("expected \"long\", not {a:?}")),
    }
}

/// Parses one line. Empty lines and `#` comments give `Ok(None)`.
pub fn parse(line: &str) -> Result<Option<Command>, String> {
    let line = line.split('#').next().unwrap_or("");
    let mut w = line.split_whitespace();
    let Some(cmd) = w.next() else {
        return Ok(None);
    };
    // The words after the command, for commands that take text.
    let rest = |line: &str| -> String {
        let after = line.trim_start();
        after[cmd.len()..].trim().to_string()
    };
    let a = w.next();
    let b = w.next();
    let c = match cmd {
        "companion" => Command::Companion(match a {
            Some("on") => CompanionCommand::On {
                capabilities: {
                    let mut caps: Vec<String> = line
                        .split_whitespace()
                        .skip(2)
                        .map(str::to_string)
                        .collect();
                    if caps.is_empty() {
                        caps.push("dictation-free".into());
                    }
                    caps
                },
            },
            Some("off") => CompanionCommand::Off,
            Some("gone") => CompanionCommand::Gone,
            _ => return Err("companion on [CAPABILITY ...]|off|gone".into()),
        }),
        "transcript" | "transcript-final" => {
            let text = rest(line);
            if text.is_empty() {
                return Err(format!("{cmd} <text>"));
            }
            Command::Companion(CompanionCommand::Transcript {
                text,
                last: cmd == "transcript-final",
            })
        }
        "dictation-error" => {
            let error = rest(line);
            if error.is_empty() {
                return Err("dictation-error <error>".into());
            }
            Command::Companion(CompanionCommand::Error { error })
        }
        "reply-result" => Command::Companion(CompanionCommand::ReplyResult {
            ok: match a {
                Some("ok") => true,
                Some("fail") => false,
                _ => return Err("reply-result ok|fail".into()),
            },
        }),
        "tap" => Command::Tap {
            fingers: fingers(a)?,
        },
        "longpress" => Command::LongPress,
        "swipe" => {
            let dir = match a {
                Some("forward") => Direction::Forward,
                Some("backward") => Direction::Backward,
                Some("down") => Direction::Down,
                Some("up") => Direction::Up,
                _ => return Err("swipe forward|backward|down|up [fingers]".into()),
            };
            Command::Swipe {
                dir,
                fingers: fingers(b)?,
            }
        }
        "scroll" => {
            let d = a
                .and_then(|a| a.parse::<f32>().ok())
                .filter(|d| d.is_finite() && d.abs() <= 1.0)
                .ok_or("scroll <displacement in pad widths, -1 to 1>")?;
            Command::Scroll { displacement: d }
        }
        "camera" => Command::Camera { long: long(a)? },
        "power" => Command::Power { long: long(a)? },
        "hinge" => match a {
            Some("open") => Command::Hinge { open: true },
            Some("closed") => Command::Hinge { open: false },
            _ => return Err("hinge open|closed".into()),
        },
        "lookup" => Command::LookUp,
        "lookdown" => Command::LookDown,
        "relative-lookup" => Command::RelativeLookUp,
        "nudge" => Command::Nudge,
        "wear" => match a {
            Some("on") => Command::Wear { on: true },
            Some("off") => Command::Wear { on: false },
            _ => return Err("wear on|off".into()),
        },
        "wink" => Command::Wink,
        "dim" => Command::Dim,
        "wait" => Command::Wait {
            ms: a
                .and_then(|a| a.parse().ok())
                .ok_or("wait <milliseconds>")?,
        },
        other => return Err(format!("unknown command {other:?}")),
    };
    Ok(Some(c))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_every_command() {
        let ok = |l: &str| parse(l).unwrap().unwrap();
        assert_eq!(ok("tap"), Command::Tap { fingers: 1 });
        assert_eq!(ok("  tap 2 "), Command::Tap { fingers: 2 });
        assert_eq!(ok("longpress"), Command::LongPress);
        assert_eq!(
            ok("swipe down 2"),
            Command::Swipe {
                dir: Direction::Down,
                fingers: 2
            }
        );
        assert_eq!(
            ok("swipe forward"),
            Command::Swipe {
                dir: Direction::Forward,
                fingers: 1
            }
        );
        assert_eq!(
            ok("scroll -0.25"),
            Command::Scroll {
                displacement: -0.25
            }
        );
        assert_eq!(ok("camera long"), Command::Camera { long: true });
        assert_eq!(ok("power"), Command::Power { long: false });
        assert_eq!(ok("hinge closed"), Command::Hinge { open: false });
        assert_eq!(ok("lookup"), Command::LookUp);
        assert_eq!(ok("lookdown"), Command::LookDown);
        assert_eq!(ok("relative-lookup"), Command::RelativeLookUp);
        assert_eq!(ok("nudge"), Command::Nudge);
        assert_eq!(ok("wear off"), Command::Wear { on: false });
        assert_eq!(ok("wink"), Command::Wink);
        assert_eq!(ok("dim"), Command::Dim);
        assert_eq!(ok("wait 250 # comment"), Command::Wait { ms: 250 });
        let comp = |c| Command::Companion(c);
        assert_eq!(
            ok("companion on"),
            comp(CompanionCommand::On {
                capabilities: vec!["dictation-free".into()]
            })
        );
        assert_eq!(
            ok("companion on location dictation-free"),
            comp(CompanionCommand::On {
                capabilities: vec!["location".into(), "dictation-free".into()]
            })
        );
        assert_eq!(ok("companion off"), comp(CompanionCommand::Off));
        assert_eq!(ok("companion gone"), comp(CompanionCommand::Gone));
        assert_eq!(
            ok("  transcript-final  buy milk  "),
            comp(CompanionCommand::Transcript {
                text: "buy milk".into(),
                last: true
            })
        );
        assert_eq!(
            ok("transcript buy"),
            comp(CompanionCommand::Transcript {
                text: "buy".into(),
                last: false
            })
        );
        assert_eq!(
            ok("dictation-error no-speech"),
            comp(CompanionCommand::Error {
                error: "no-speech".into()
            })
        );
        assert_eq!(
            ok("reply-result fail"),
            comp(CompanionCommand::ReplyResult { ok: false })
        );
    }

    #[test]
    fn blank_and_bad_lines() {
        assert_eq!(parse("").unwrap(), None);
        assert_eq!(parse("   # only a comment").unwrap(), None);
        for bad in [
            "tap 4",
            "tap x",
            "swipe",
            "swipe left",
            "scroll",
            "scroll 2",
            "camera short",
            "hinge",
            "wear",
            "wait",
            "fly",
            "companion",
            "companion maybe",
            "transcript",
            "dictation-error",
            "reply-result maybe",
        ] {
            assert!(parse(bad).is_err(), "{bad}");
        }
    }
}
