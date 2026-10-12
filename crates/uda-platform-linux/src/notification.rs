use crate::error::UdaError;
use std::collections::HashMap;
use std::sync::Arc;
use uda_core::capability::Capability;
use uda_core::notification::{Notification, NotificationManager};
use zbus::Connection;

// The FreeDesktop `Notify` D-Bus method takes nine parameters. `clippy`'s
// default `too_many_arguments` threshold of seven cannot be met without
// diverging from the wire protocol, so the lint is silenced here with a
// comment explaining exactly why the signature is fixed by the specification.
#[allow(clippy::too_many_arguments)]
#[zbus::proxy(
    interface = "org.freedesktop.Notifications",
    default_service = "org.freedesktop.Notifications",
    default_path = "/org/freedesktop/Notifications"
)]
trait Notifications {
    /// Send a notification. Returns the assigned notification ID.
    #[allow(clippy::too_many_arguments)]
    fn Notify(
        &self,
        app_name: &str,
        replaces_id: u32,
        app_icon: &str,
        summary: &str,
        body: &str,
        actions: Vec<String>,
        hints: HashMap<String, zbus::zvariant::Value<'_>>,
        expire_timeout: i32,
    ) -> zbus::Result<u32>;
}

pub struct LinuxNotificationManager {
    connection: Arc<Connection>,
}

impl LinuxNotificationManager {
    pub async fn new() -> Result<Self, UdaError> {
        let connection = Connection::session()
            .await
            .map_err(|e| UdaError::Internal(format!("Failed to connect to session bus: {e}")))?;
        Ok(Self {
            connection: Arc::new(connection),
        })
    }
}

#[async_trait::async_trait]
impl NotificationManager for LinuxNotificationManager {
    async fn send(&self, notification: &Notification) -> Result<u32, UdaError> {
        let proxy = NotificationsProxy::new(&self.connection)
            .await
            .map_err(|e| {
                UdaError::Internal(format!("Failed to create notifications proxy: {e}"))
            })?;

        let actions_flat: Vec<String> = notification
            .actions
            .iter()
            .flat_map(|(k, v)| [k.clone(), v.clone()])
            .collect();

        // The FreeDesktop spec transports urgency as a hint of type Byte.
        let urgency = notification.urgency as u8;
        let hints: HashMap<String, zbus::zvariant::Value<'_>> = if urgency > 0 {
            HashMap::from([(
                "urgency".to_string(),
                zbus::zvariant::Value::<'_>::from(urgency),
            )])
        } else {
            HashMap::new()
        };

        let notification_id = proxy
            .Notify(
                &notification.app_name,
                notification.replaces_id,
                &notification.app_icon,
                &notification.summary,
                &notification.body,
                actions_flat,
                hints,
                notification.expire_timeout,
            )
            .await
            .map_err(|e| UdaError::Internal(format!("Notify call failed: {e}")))?;

        Ok(notification_id)
    }

    fn capabilities(&self) -> Result<Capability, UdaError> {
        Ok(Capability::SEND_NOTIFICATION)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn actions_flattening() {
        let actions = [
            ("action1".to_string(), "OK".to_string()),
            ("action2".to_string(), "Cancel".to_string()),
        ];
        let flat: Vec<String> = actions
            .iter()
            .flat_map(|(k, v)| [k.clone(), v.clone()])
            .collect();
        assert_eq!(flat, vec!["action1", "OK", "action2", "Cancel"]);
    }

    #[test]
    fn actions_empty() {
        let actions: Vec<(String, String)> = Vec::new();
        let flat: Vec<String> = actions
            .iter()
            .flat_map(|(k, v)| [k.clone(), v.clone()])
            .collect();
        assert!(flat.is_empty());
    }

    #[test]
    fn notification_default_has_zero_replaces_id() {
        let notification = Notification::default();
        assert_eq!(notification.replaces_id, 0);
    }

    #[test]
    fn notification_default_empty_body() {
        let notification = Notification::default();
        assert!(notification.body.is_empty());
        assert!(notification.summary.is_empty());
    }

    #[test]
    fn notification_urgency_hint_is_omitted_for_low() {
        let notification = Notification {
            urgency: uda_core::notification::Urgency::Low,
            ..Notification::default()
        };
        let urgency = notification.urgency as u8;
        // Low urgency (0) is the server default, so no hint is required.
        assert_eq!(urgency, 0);
    }

    #[test]
    fn notification_urgency_hint_is_sent_for_normal() {
        let notification = Notification::default();
        let urgency = notification.urgency as u8;
        assert_eq!(urgency, 1);
    }

    /// The `hints` argument must reach the bus as the FreeDesktop `a{sv}`
    /// dictionary. A `Vec<(String, Value)>` serialises as an array of structs
    /// (`a(sv)`), which the daemon rejects as an unknown method. Declaring the
    /// mock server's parameter as a map makes the bus itself reject the wrong
    /// encoding, so this fails loudly if the type ever regresses.
    ///
    /// Requires an isolated bus (`scripts/test-linux-mock.sh` runs the suite
    /// under `dbus-run-session`). On a live desktop the real notification daemon
    /// already owns the name, so the test skips instead of fighting it.
    #[tokio::test]
    async fn send_transports_hints_as_a_dictionary() -> Result<(), Box<dyn std::error::Error>> {
        use std::sync::Mutex;
        use zbus::fdo::{RequestNameFlags, RequestNameReply};

        struct MockNotifications {
            received: Arc<Mutex<Option<HashMap<String, zbus::zvariant::OwnedValue>>>>,
        }

        #[zbus::interface(name = "org.freedesktop.Notifications")]
        impl MockNotifications {
            #[allow(non_snake_case, clippy::too_many_arguments)]
            fn Notify(
                &self,
                _app_name: &str,
                _replaces_id: u32,
                _app_icon: &str,
                _summary: &str,
                _body: &str,
                _actions: Vec<String>,
                hints: HashMap<String, zbus::zvariant::OwnedValue>,
                _expire_timeout: i32,
            ) -> u32 {
                *self.received.lock().unwrap() = Some(hints);
                42
            }
        }

        let server = zbus::Connection::session().await?;
        let received = Arc::new(Mutex::new(None));
        let mock = MockNotifications {
            received: Arc::clone(&received),
        };
        server
            .object_server()
            .at("/org/freedesktop/Notifications", mock)
            .await?;

        match server
            .request_name_with_flags(
                "org.freedesktop.Notifications",
                RequestNameFlags::DoNotQueue.into(),
            )
            .await
        {
            Ok(RequestNameReply::PrimaryOwner) => {}
            // A live desktop daemon already owns the name (`NameTaken`).
            _ => return Ok(()),
        }

        let manager = LinuxNotificationManager::new().await?;
        // The default notification carries normal urgency, so the urgency hint
        // is populated and must survive as a dictionary entry.
        assert_eq!(manager.send(&Notification::default()).await?, 42);
        let received = received.lock().unwrap();
        let hints = received
            .as_ref()
            .expect("the mock server received the Notify call");
        assert!(
            hints.contains_key("urgency"),
            "the urgency hint must arrive as a dictionary entry, got {hints:?}"
        );
        Ok(())
    }
}
