//! # orchard-core
//!
//! Domain types and verification logic shared by every Orchard component:
//! the catalog model ([`model`]), the AppStream ingest ([`appstream`]), the
//! OCI artifact verifier ([`oci`]), and the permission translator
//! ([`permissions`]).

pub mod appstream;
pub mod oci_asset;
pub mod model;
pub mod oci;
pub mod permissions;
pub mod proof;
pub mod settings;

pub use appstream::*;
pub use model::*;
pub use oci::*;
pub use permissions::*;
pub use settings::{Settings, SettingsHandle};

/// The platform tag of the machine we run on, e.g. `linux-x86_64`.
/// This is the resolution key for releases and assets: a listing may
/// declare coverage for many platforms, and the device picks the
/// artifact built for its own.
pub fn host_platform() -> String {
    format!("linux-{}", std::env::consts::ARCH)
}

/// The host architecture alone (`x86_64`, `aarch64`), as it appears
/// inside a flatpak ref.
pub fn host_ref_arch() -> String {
    std::env::consts::ARCH.to_string()
}
