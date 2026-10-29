//! The address-space figures a native host build reports: none.
//!
//! The PE build walks its own address space because a 32-bit game runs out of
//! it. A native host build is a 64-bit process, and the application's memory
//! is the host's to account for, so there is nothing to walk; the watch that
//! reads these starts no thread on a 64-bit build.

use mtld3d_core::address_space::FreeSpace;

/// No regions: a native host build does not walk its address space.
#[must_use]
pub const fn free_space() -> FreeSpace {
    FreeSpace::new()
}

/// What the address-space map reads on a native host build.
#[must_use]
pub fn address_space_map() -> String {
    String::from("not walked on a native host")
}
