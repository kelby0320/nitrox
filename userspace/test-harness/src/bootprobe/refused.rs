//! **The boot probe's raw calls**: each passes what a safe wrapper cannot express, or reads an
//! answer libos folds, so the call is the test. `check-syscalls` counts this module on its own.

pub(crate) mod authority;
pub(crate) mod deadlines;
pub(crate) mod restrict;
