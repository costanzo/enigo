mod win_impl;
pub use win_impl::Enigo;

use crate::PermissionStatus;

/// Windows does not expose one durable capture-authorization prompt.
#[must_use]
pub const fn capture_permission(_request: bool) -> PermissionStatus {
    PermissionStatus::Unknown
}

/// Windows input availability depends on the target process integrity level.
#[must_use]
pub const fn input_permission(_request: bool) -> PermissionStatus {
    PermissionStatus::Unknown
}
