//! The Glass end of the everyday companion link: a BLE peripheral on a raw HCI UART.
//!
//! `omg-bled` owns `/dev/ttyO1`, loads the firmware patch, advertises one GATT service, and hands
//! each phone session to glassd as a plain Unix stream. Nothing above that stream knows what
//! carries it. See `docs/ble-link.md`.
//!
//! The logic has no I/O: [`engine::Engine`] takes bytes and the time and returns bytes and
//! actions, so all of it runs on a host against [`fake::FakeController`]. Only `main.rs` touches
//! a descriptor.

pub mod att;
pub mod bringup;
pub mod engine;
pub mod fake;
pub mod hci;
