//! Independent WeType transport. No WeChat process or account access.
mod auth;
mod crypto;
mod ecdh;
mod machine;
mod transport;
pub use auth::{Client, Device};
pub use transport::{Response, Transport, request};

pub fn preflight() -> crate::Result<()> {
    preflight_cancellable(&|| false)
}

pub fn preflight_cancellable(cancelled: &dyn Fn() -> bool) -> crate::Result<()> {
    let mut client = Client::from_default_device_cancellable(cancelled)?;
    client.close();
    Ok(())
}
