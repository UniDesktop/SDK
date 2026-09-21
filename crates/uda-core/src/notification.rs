use crate::capability::Capability;
use crate::error::UdaError;

/// Urgency level for a notification, following the FreeDesktop Notifications spec.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Urgency {
    /// Low urgency. No special display.
    Low = 0,
    /// Normal urgency. Default behavior.
    #[default]
    Normal = 1,
    /// Critical urgency. Displayed immediately, may override screen lock.
    Critical = 2,
}

/// Represents a desktop notification to be sent via the system notification service.
///
/// Maps directly to the FreeDesktop `org.freedesktop.Notifications.Notify` parameters.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Notification {
    /// The application name (sender).
    pub app_name: String,
    /// Optional ID of a notification to replace. 0 means a new notification.
    pub replaces_id: u32,
    /// Path or URI to an icon image.
    pub app_icon: String,
    /// One-line summary / title of the notification.
    pub summary: String,
    /// Multi-line body of the notification.
    pub body: String,
    /// Action pairs (key, localized_label). Flattened for transport internally.
    pub actions: Vec<(String, String)>,
    /// Timeout in milliseconds. 0 = server default, -1 = never expire.
    pub expire_timeout: i32,
    /// Urgency level, transported as the `urgency` hint.
    pub urgency: Urgency,
}

impl Default for Notification {
    fn default() -> Self {
        Self {
            app_name: "UDA".to_string(),
            replaces_id: 0,
            app_icon: String::new(),
            summary: String::new(),
            body: String::new(),
            actions: Vec::new(),
            expire_timeout: 0,
            urgency: Urgency::default(),
        }
    }
}

/// Cross-platform notification management interface.
#[async_trait::async_trait]
pub trait NotificationManager {
    /// Send a notification and return the notification ID assigned by the server.
    async fn send(&self, notification: &Notification) -> Result<u32, UdaError>;

    /// Return the set of capabilities supported by the current backend.
    fn capabilities(&self) -> Result<Capability, UdaError>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn urgency_default_is_normal() {
        assert_eq!(Urgency::default(), Urgency::Normal);
    }

    #[test]
    fn urgency_values_match_spec() {
        assert_eq!(Urgency::Low as u8, 0);
        assert_eq!(Urgency::Normal as u8, 1);
        assert_eq!(Urgency::Critical as u8, 2);
    }

    #[test]
    fn notification_default_has_normal_urgency() {
        let n = Notification::default();
        assert_eq!(n.urgency, Urgency::Normal);
    }

    #[test]
    fn notification_default_has_zero_replaces_id() {
        let n = Notification::default();
        assert_eq!(n.replaces_id, 0);
    }
}
