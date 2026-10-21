//! Display back ends. Milestone 1 has only a logging back end; blanking through `FBIOBLANK`
//! or Android early suspend comes in milestone 2 (docs/specs/kernel-interfaces.md §4.3).

use glass_api::proto::Screen;

/// Turns the panel on, dims it, or turns it off.
pub trait Display {
    fn set(&mut self, screen: Screen);
}

/// Does nothing but log the requested state.
#[derive(Debug, Default)]
pub struct LoggingDisplay;

impl Display for LoggingDisplay {
    fn set(&mut self, screen: Screen) {
        crate::info!("display -> {screen:?} (logging back end, nothing changed)");
    }
}
