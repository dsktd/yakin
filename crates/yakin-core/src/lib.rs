pub mod auto;
pub mod desktop;
pub mod device;
pub mod inherit;
pub mod keyboard;
pub mod layer;
pub mod lizard;
pub mod menu;
pub mod output;
pub mod power;
pub mod profile;
pub mod sleep;
pub mod state;

pub type Result<T> = anyhow::Result<T>;
