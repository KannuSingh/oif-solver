//! Shinobi Cash cross-chain intent types.
//!
//! This module defines types specific to the Shinobi Cash protocol, which extends
//! the OIF StandardOrder with bidirectional oracles and custom refund logic.
//!
//! Key types:
//! - `ShinobiIntent`: Extended StandardOrder for both withdrawals and deposits
//! - Settler interface bindings for InputSettler and OutputSettler contracts

pub mod intent;
pub mod settler;

pub use intent::ShinobiIntent;
pub use settler::{IShinobiInputSettler, IShinobiOutputSettler, ShinobiIntentSol};
