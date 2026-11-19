//! The egress policy for the calls Crucible makes to addresses its users configure (contracts 1.7 and 12.6):
//! notification webhooks today.
//!
//! - [`classify`] says what kind of network an address belongs to, in every spelling of the address.
//! - [`EgressPolicy`] decides: cloud metadata addresses are always refused; everything else is allowed by
//!   default, private networks included; `egress.mode = strict` is the operator's switch for `https` only and
//!   public addresses only, with `egress.allow_cidrs` as its exceptions. Every [`Refusal`] names the setting
//!   that changes it.
//! - [`Egress`] makes the call: it resolves through `Net`, checks every resolved address, connects to the
//!   checked address, follows no redirect and keeps tight time limits.

mod addr;
mod client;
mod policy;

pub use addr::{AddrClass, Cidr, InvalidCidr, classify, embedded_v4};
pub use client::{BODY_LIMIT, CONNECT_TIMEOUT, Egress, EgressError, RESPONSE_TIMEOUT, Reply};
pub use policy::{EgressPolicy, Mode, Refusal, Target};
