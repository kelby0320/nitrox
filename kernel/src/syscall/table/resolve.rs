//! What a resolve does beyond resolving: `sys_ns_lookup`'s fifth argument, read.

use super::*;
use crate::libkern::resolve_op::{
    RESOLVE_OP_CREATE, RESOLVE_OP_FLUSH, RESOLVE_OP_GROW, RESOLVE_OP_HELD, RESOLVE_OP_RENAME, RESOLVE_OP_TRUNCATE,
    ResolveOp,
};

/// What a resolve does beyond resolving, read from its [`ResolveOp`].
#[derive(Copy, Clone)]
pub(super) enum Resolve {
    Plain,
    Size(u64, SizeChange),
    /// Both paths resolve through **one binding**: no server sees a cross-filesystem rename.
    Rename {
        dest_ptr: u64,
        dest_len: usize,
        flags: u64,
    },
    Flush,
    Held,
}

impl Resolve {
    /// The [`ResolveOp`] at `op_ptr`, `0` a plain lookup; strict about the fields a code does not use.
    pub(super) fn read(op_ptr: u64) -> Result<Resolve, KError> {
        if op_ptr == 0 {
            return Ok(Resolve::Plain);
        }
        let op =
            copy_from_user(UserPtr::<ResolveOp>::new(op_ptr).map_err(from_user_access)?).map_err(from_user_access)?;
        let unused = |flags: bool, size: bool, dest: bool| {
            (flags && op.flags != 0) || (size && op.size != 0) || (dest && (op.dest_ptr != 0 || op.dest_len != 0))
        };
        let (resolve, stray) = match op.code {
            RESOLVE_OP_CREATE => (Resolve::Size(op.size, SizeChange::Create), unused(true, false, true)),
            RESOLVE_OP_GROW => (Resolve::Size(op.size, SizeChange::Grow), unused(true, false, true)),
            RESOLVE_OP_TRUNCATE => (Resolve::Size(op.size, SizeChange::Truncate), unused(true, false, true)),
            RESOLVE_OP_RENAME => {
                let rename =
                    Resolve::Rename { dest_ptr: op.dest_ptr, dest_len: op.dest_len as usize, flags: op.flags as u64 };
                (rename, unused(false, true, false))
            }
            RESOLVE_OP_FLUSH => (Resolve::Flush, unused(true, true, true)),
            RESOLVE_OP_HELD => (Resolve::Held, unused(true, true, true)),
            _ => return Err(KError::InvalidArgument),
        };
        if stray { Err(KError::InvalidArgument) } else { Ok(resolve) }
    }

    /// The request the wire carries, or `None` for a flush or held, which goes no further than the
    /// kernel and asks for no rights.
    pub(super) fn forward(self, rights_bits: u64) -> Result<Option<ForwardOp>, KError> {
        match self {
            Resolve::Flush | Resolve::Held if rights_bits != 0 => Err(KError::InvalidArgument),
            Resolve::Flush | Resolve::Held => Ok(None),
            _ => self.forward_op().map(Some),
        }
    }

    /// The request the wire carries, or the refusal of a size or flags it cannot: a size past
    /// `u32::MAX` is `TooLarge`, not that size modulo 2^32.
    pub(super) fn forward_op(self) -> Result<ForwardOp, KError> {
        Ok(match self {
            Resolve::Plain => ForwardOp::Plain,
            Resolve::Size(size, change) => ForwardOp::Size(u32::try_from(size).map_err(|_| KError::TooLarge)?, change),
            Resolve::Rename { flags, .. } => {
                ForwardOp::Rename { flags: u16::try_from(flags).map_err(|_| KError::InvalidArgument)? }
            }
            Resolve::Flush | Resolve::Held => return Err(KError::InvalidArgument),
        })
    }
}
