//! Pure decision roles shared by isolated provider and UI boundaries.

/// The effect of an approval action, independent of its displayed payload.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ApprovalRole {
    Once,
    Session,
    Decline,
    Cancel,
    DenyPermissions,
    PolicyAllow,
    PolicyDeny,
}

/// Action IDs are opaque to callers. The adapter stamps known wire semantics
/// into its own IDs so views never infer permission from labels or arguments.
/// Old unclassified IDs deliberately provide no keyboard authority.
pub fn approval_role(id: &str) -> Option<ApprovalRole> {
    use ApprovalRole::*;
    Some(match id {
        "accept" | "grant-turn" => Once,
        "acceptForSession" | "grant-session" => Session,
        "decline" => Decline,
        "cancel" => Cancel,
        "deny" => DenyPermissions,
        _ => {
            let (index, role) = id.strip_prefix("decision-")?.split_once(':')?;
            index.parse::<usize>().ok()?;
            match role {
                "once" => Once,
                "session" => Session,
                "decline" => Decline,
                "cancel" => Cancel,
                "policy-allow" => PolicyAllow,
                "policy-deny" => PolicyDeny,
                _ => return None,
            }
        }
    })
}
