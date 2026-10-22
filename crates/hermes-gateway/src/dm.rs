//! DM (Direct Message) pairing mechanism (Requirement 7.9).
//!
//! Handles authorization decisions when an unregistered user sends a
//! direct message to the bot. Supports configurable behaviors:
//! - Pair: Create a session and request admin approval
//! - Ignore: Silently discard the message

use serde::{Deserialize, Serialize};
use std::collections::HashSet;

use hermes_config::UnauthorizedDmBehavior;

// ---------------------------------------------------------------------------
// DmDecision
// ---------------------------------------------------------------------------

/// Decision outcome for a DM from an unregistered/unauthorized user.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DmDecision {
    /// Allow the DM through (user is authorized).
    Allow,
    /// Pair the user: create a session and request admin approval.
    Pair {
        /// A message to show the user while awaiting approval.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        message: Option<String>,
    },
    /// Deny the DM entirely.
    Deny,
}

// ---------------------------------------------------------------------------
// DmManager
// ---------------------------------------------------------------------------

/// Manages DM authorization decisions for incoming messages.
pub struct DmManager {
    /// Set of user IDs that are explicitly authorized to DM the bot.
    authorized_users: HashSet<String>,

    /// Set of user IDs that have admin privileges.
    admin_users: HashSet<String>,

    /// How to handle DMs from unauthorized users (global default).
    unauthorized_dm_behavior: UnauthorizedDmBehavior,

    /// Per-platform override of `unauthorized_dm_behavior`, keyed by platform
    /// name (e.g. "telegram"). Falls back to the global default when absent.
    platform_behaviors: std::collections::HashMap<String, UnauthorizedDmBehavior>,
}

impl DmManager {
    /// Create a new `DmManager`.
    pub fn new(
        authorized_users: HashSet<String>,
        admin_users: HashSet<String>,
        unauthorized_dm_behavior: UnauthorizedDmBehavior,
    ) -> Self {
        Self {
            authorized_users,
            admin_users,
            unauthorized_dm_behavior,
            platform_behaviors: std::collections::HashMap::new(),
        }
    }

    /// Create a `DmManager` with the Pair behavior and no pre-authorized users.
    pub fn with_pair_behavior() -> Self {
        Self {
            authorized_users: HashSet::new(),
            admin_users: HashSet::new(),
            unauthorized_dm_behavior: UnauthorizedDmBehavior::Pair,
            platform_behaviors: std::collections::HashMap::new(),
        }
    }

    /// Create a `DmManager` with the Ignore behavior and no pre-authorized users.
    pub fn with_ignore_behavior() -> Self {
        Self {
            authorized_users: HashSet::new(),
            admin_users: HashSet::new(),
            unauthorized_dm_behavior: UnauthorizedDmBehavior::Ignore,
            platform_behaviors: std::collections::HashMap::new(),
        }
    }

    /// Build a `DmManager` from platform configs.
    ///
    /// For each enabled platform:
    /// - `unauthorized_dm_behavior` is registered as a per-platform override.
    /// - `admin_users` are added as admins.
    /// - `allowed_users` (union of the first-class field and
    ///   `extra["allowed_users"]`) are pre-authorized (the operator explicitly
    ///   vetted them, so DM pairing approval is redundant).
    pub fn from_platform_configs(
        platforms: &std::collections::HashMap<String, hermes_config::PlatformConfig>,
    ) -> Self {
        let mut mgr = Self::with_pair_behavior();
        for (name, pc) in platforms {
            if !pc.enabled {
                continue;
            }
            mgr.set_platform_behavior(name, pc.unauthorized_dm_behavior);
            for admin in &pc.admin_users {
                mgr.add_admin(admin.clone());
            }
            for user in pc.allowed_users_union() {
                mgr.authorize_user(user);
            }
        }
        mgr
    }

    /// Handle an incoming DM from a user on a platform.
    ///
    /// Returns a `DmDecision` indicating how to proceed:
    /// - `Allow` if the user is authorized or is an admin
    /// - `Pair` if unauthorized and behavior is Pair
    /// - `Deny` if unauthorized and behavior is Ignore
    pub async fn handle_dm(&self, user_id: &str, platform: &str) -> DmDecision {
        // Admins are always allowed
        if self.admin_users.contains(user_id) {
            return DmDecision::Allow;
        }

        // Authorized users are always allowed
        if self.authorized_users.contains(user_id) {
            return DmDecision::Allow;
        }

        // Unauthorized user: apply the platform-specific behavior, falling
        // back to the global default.
        let behavior = self
            .platform_behaviors
            .get(platform)
            .copied()
            .unwrap_or(self.unauthorized_dm_behavior);
        match behavior {
            UnauthorizedDmBehavior::Pair => DmDecision::Pair {
                message: Some(
                    "Your request has been submitted for approval. You will be notified once an admin reviews it.".to_string(),
                ),
            },
            UnauthorizedDmBehavior::Ignore => DmDecision::Deny,
        }
    }

    /// Set a per-platform override for the unauthorized-DM behavior.
    pub fn set_platform_behavior(
        &mut self,
        platform: impl Into<String>,
        behavior: UnauthorizedDmBehavior,
    ) {
        self.platform_behaviors.insert(platform.into(), behavior);
    }

    /// Add a user to the authorized users set.
    pub fn authorize_user(&mut self, user_id: impl Into<String>) {
        self.authorized_users.insert(user_id.into());
    }

    /// Remove a user from the authorized users set.
    pub fn deauthorize_user(&mut self, user_id: &str) {
        self.authorized_users.remove(user_id);
    }

    /// Add a user to the admin users set.
    pub fn add_admin(&mut self, user_id: impl Into<String>) {
        self.admin_users.insert(user_id.into());
    }

    /// Remove a user from the admin users set.
    pub fn remove_admin(&mut self, user_id: &str) {
        self.admin_users.remove(user_id);
    }

    /// Check if a user is authorized.
    pub fn is_authorized(&self, user_id: &str) -> bool {
        self.authorized_users.contains(user_id) || self.admin_users.contains(user_id)
    }

    /// Check if a user is an admin.
    pub fn is_admin(&self, user_id: &str) -> bool {
        self.admin_users.contains(user_id)
    }

    /// Get the number of authorized users.
    pub fn authorized_user_count(&self) -> usize {
        self.authorized_users.len()
    }

    /// Get the number of admin users.
    pub fn admin_user_count(&self) -> usize {
        self.admin_users.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn dm_manager_allows_authorized_user() {
        let mut dm = DmManager::with_ignore_behavior();
        dm.authorize_user("user1");

        let decision = dm.handle_dm("user1", "telegram").await;
        assert_eq!(decision, DmDecision::Allow);
    }

    #[tokio::test]
    async fn dm_manager_per_platform_behavior_override() {
        let mut dm = DmManager::with_pair_behavior();
        dm.set_platform_behavior("telegram", UnauthorizedDmBehavior::Ignore);

        // telegram: Ignore → Deny (no pairing message)
        let decision = dm.handle_dm("stranger", "telegram").await;
        assert_eq!(decision, DmDecision::Deny);

        // other platform: falls back to global Pair
        let decision = dm.handle_dm("stranger", "discord").await;
        assert!(matches!(decision, DmDecision::Pair { .. }));
    }

    #[tokio::test]
    async fn dm_manager_from_platform_configs() {
        use std::collections::HashMap;
        let mut platforms = HashMap::new();
        let mut pc = hermes_config::PlatformConfig::default();
        pc.enabled = true;
        pc.unauthorized_dm_behavior = UnauthorizedDmBehavior::Ignore;
        pc.admin_users = vec!["admin1".to_string()];
        pc.allowed_users = vec!["565903951".to_string()];
        platforms.insert("telegram".to_string(), pc);

        let dm = DmManager::from_platform_configs(&platforms);

        // allowed_users pre-authorized → Allow (no pairing)
        let decision = dm.handle_dm("565903951", "telegram").await;
        assert_eq!(decision, DmDecision::Allow);

        // admin → Allow
        let decision = dm.handle_dm("admin1", "telegram").await;
        assert_eq!(decision, DmDecision::Allow);

        // stranger on telegram → per-platform Ignore → Deny
        let decision = dm.handle_dm("stranger", "telegram").await;
        assert_eq!(decision, DmDecision::Deny);
    }

    #[tokio::test]
    async fn dm_manager_allows_admin() {
        let mut dm = DmManager::with_ignore_behavior();
        dm.add_admin("admin1");

        let decision = dm.handle_dm("admin1", "discord").await;
        assert_eq!(decision, DmDecision::Allow);
    }

    #[tokio::test]
    async fn dm_manager_pair_behavior() {
        let dm = DmManager::with_pair_behavior();
        let decision = dm.handle_dm("unknown_user", "telegram").await;
        assert!(matches!(decision, DmDecision::Pair { .. }));
    }

    #[tokio::test]
    async fn dm_manager_ignore_behavior() {
        let dm = DmManager::with_ignore_behavior();
        let decision = dm.handle_dm("unknown_user", "telegram").await;
        assert_eq!(decision, DmDecision::Deny);
    }

    #[tokio::test]
    async fn dm_manager_authorize_and_deauthorize() {
        let mut dm = DmManager::with_ignore_behavior();
        dm.authorize_user("user1");
        assert!(dm.is_authorized("user1"));

        dm.deauthorize_user("user1");
        assert!(!dm.is_authorized("user1"));
    }

    #[test]
    fn dm_decision_serde() {
        let allow = DmDecision::Allow;
        let json = serde_json::to_string(&allow).unwrap();
        assert!(json.contains("allow"));

        let pair = DmDecision::Pair {
            message: Some("pending".to_string()),
        };
        let json = serde_json::to_string(&pair).unwrap();
        assert!(json.contains("pair"));

        let deny = DmDecision::Deny;
        let json = serde_json::to_string(&deny).unwrap();
        assert!(json.contains("deny"));
    }
}
