//! What `sys_memory_map` maps: a memory object, eagerly, or a file, a page at a time.

use super::*;

/// The two kinds a mapping takes.
pub(super) enum Mappable {
    Memory(Ref<MemoryObject>),
    File(Ref<FileObject>),
}

impl Mappable {
    /// `object` as what a mapping takes, else `InvalidHandle`: no other kind carries a `MAP_*`
    /// right.
    pub(super) fn of(object: ObjectRef) -> Result<Self, KError> {
        match object.downcast::<MemoryObject>() {
            Ok(memory) => Ok(Mappable::Memory(memory)),
            Err(other) => other.downcast().map(Mappable::File).map_err(|_| KError::InvalidHandle),
        }
    }
}
