//! Policy layer vocabulary: source tiers, authority, ownership, and
//! tighten-only pins.

use std::path::{Path, PathBuf};

/// Trust tier of a policy layer; lower = higher authority (mdm > system > user,
/// vendor last); derives authority + ownership; first-wins applies in tier order.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(super) enum PolicyLayerTier {
    Mdm,
    SystemRequirements,
    SystemManaged,
    UserRequirements,
    UserManaged,
    /// The Claude `managed-settings.json`; sorts (applies) after every grok
    /// layer.
    Vendor,
}

impl PolicyLayerTier {
    /// Vendor files configure Claude, not grok — advisory to grok-native
    /// subjects.
    pub fn authority(self) -> PolicySourceAuthority {
        match self {
            Self::Vendor => PolicySourceAuthority::Advisory,
            _ => PolicySourceAuthority::Native,
        }
    }

    /// Who can write the layer: MDM/system TOML and the root-owned vendor
    /// file are admin-controlled; `~/.grok` layers are user-writable.
    pub fn ownership(self) -> PolicyLayerOwnership {
        match self {
            Self::UserRequirements | Self::UserManaged => PolicyLayerOwnership::User,
            Self::Mdm | Self::SystemRequirements | Self::SystemManaged | Self::Vendor => {
                PolicyLayerOwnership::Admin
            }
        }
    }
}

/// One TOML policy layer. The vendor JSON layer ([`PolicyLayerTier::Vendor`])
/// is applied separately: its value is already JSON and skips the TOML policy
/// key filter.
pub(super) struct PolicyLayer {
    pub(super) tier: PolicyLayerTier,
    pub(super) path: PathBuf,
    pub(super) value: toml::Value,
}

/// Whether a source binds everything (grok's own TOML layers, `Native`) or only
/// foreign-defined subjects (the vendor Claude managed-settings.json, `Advisory`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum PolicySourceAuthority {
    #[default]
    Native,
    Advisory,
}

/// Tighten-only pin: unpinned, or disabled by a named layer (`true` never
/// un-pins).
#[derive(Debug, Clone, Default)]
pub enum PolicyPin {
    #[default]
    Unpinned,
    Disabled {
        source: PathBuf,
        /// Who can write the pinning layer (see [`PolicyLayerOwnership`]);
        /// carried in the pin so pin and grant rule can never desync.
        ownership: PolicyLayerOwnership,
    },
}

impl PolicyPin {
    pub fn is_disabled(&self) -> bool {
        matches!(self, Self::Disabled { .. })
    }

    /// The policy layer that pinned this off, if any.
    pub fn source(&self) -> Option<&Path> {
        match self {
            Self::Unpinned => None,
            Self::Disabled { source, .. } => Some(source),
        }
    }
}

/// Who can write the layer a policy value came from: `Admin` restrictions accept
/// only admin-owned exception grants; `User` restrictions accept any. A user-writable
/// grant that satisfied an admin lockdown would let the restricted user lift it themselves.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PolicyLayerOwnership {
    Admin,
    User,
}

impl PolicyLayerOwnership {
    /// Whether a restriction owned by `self` accepts an exception grant owned by `grant`.
    pub fn accepts_grant_from(self, grant: PolicyLayerOwnership) -> bool {
        self == PolicyLayerOwnership::User || grant == PolicyLayerOwnership::Admin
    }
}
