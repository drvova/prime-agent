//! All modules are `pub(crate)` internals of the tools subsystem; the
//! crate facade (crate root) re-exports only the public tool surface.

pub(crate) mod kernel_cell;
#[cfg_attr(
    windows,
    expect(
        dead_code,
        reason = "on Windows the unix `posix_join` arm of the path helpers has no caller"
    )
)]
pub(crate) mod path_utils;
pub(crate) mod render_utils;
pub(crate) mod scheme;
pub(crate) mod tool_definition;
