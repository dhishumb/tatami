//! Arena keys. Generational, so a stale id held across a window close can
//! never alias a newer node.

slotmap::new_key_type! {
    pub struct WindowId;
    pub struct NodeId;
    pub struct WorkspaceId;
    pub struct MonitorId;
}
