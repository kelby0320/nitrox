//! The static checks: no guest, each failing with what to fix.

mod abi;
mod arch;
mod comment_budget;
pub(crate) mod comments;
mod deferrals;
pub(crate) mod docs;
mod images;
mod irq_scope;
pub(crate) mod moves;
mod nightly;
pub(crate) mod size;

pub(crate) use abi::*;
pub(crate) use arch::*;
pub(crate) use deferrals::*;
pub(crate) use images::*;
pub(crate) use irq_scope::*;
pub(crate) use nightly::*;
