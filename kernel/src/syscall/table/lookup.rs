//! Handle lookups that check the type of the object found.

use super::*;
use crate::object::header::KObject;

/// Look up `h` for `pid`, require `required` rights, confirm the object is of type `expected`
/// (else `InvalidArgument`); the [`LookupOk`] pins the object.
pub(super) fn lookup_typed(h: u64, pid: u32, required: Rights, expected: KObjectType) -> Result<LookupOk, KError> {
    let ok = global::get().lookup(RawHandle(h), pid, required).map_err(map_handle_err)?;
    if ok.object.object_type() != expected {
        return Err(KError::InvalidArgument);
    }
    Ok(ok)
}

/// Look up `h` for `pid`, require `required` rights, and the object a `T` (else `InvalidArgument`).
pub(super) fn lookup_as<T: KObject>(h: u64, pid: u32, required: Rights) -> Result<Ref<T>, KError> {
    global::get().lookup_as(RawHandle(h), pid, required).map_err(map_handle_err)
}
