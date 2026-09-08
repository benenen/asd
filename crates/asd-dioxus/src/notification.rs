//! Native effects accept metadata only; terminal bytes never enter this API.
use asd_client::attention::AttentionKind;

pub(crate) struct NotificationMessage {
    pub host_label: String,
    pub session_name: String,
    pub attention: AttentionKind,
}

pub(crate) trait NotificationAdapter {
    fn show(&self, message: &NotificationMessage) -> Result<(), String>;
}

pub(crate) struct NativeNotificationAdapter;

/// Strip control/bidi characters and markup so remote host labels cannot
/// inject terminal controls or desktop notification markup.
fn clean(text: &str) -> String {
    text.chars()
        .filter(|c| {
            !c.is_control() && !matches!(c, '\u{202a}'..='\u{202e}' | '\u{2066}'..='\u{2069}')
        })
        .take(160)
        .collect::<String>()
        .replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

impl NotificationAdapter for NativeNotificationAdapter {
    fn show(&self, message: &NotificationMessage) -> Result<(), String> {
        let state = match message.attention {
            AttentionKind::Done => "Done",
            AttentionKind::NeedsAttention => "Needs attention",
        };
        crate::platform::notify(
            &format!("asd — {state}"),
            &format!(
                "{} / {} — {state}",
                clean(&message.host_label),
                clean(&message.session_name)
            ),
        )
    }
}

pub(crate) fn deliver(
    adapter: &dyn NotificationAdapter,
    host_label: &str,
    effects: Vec<asd_client::attention::AttentionEffect>,
) {
    for effect in effects {
        let message = NotificationMessage {
            host_label: host_label.into(),
            session_name: effect.name,
            attention: effect.kind,
        };
        if let Err(error) = adapter.show(&message) {
            tracing::warn!(%error, "attention notification failed");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::RefCell;
    #[test]
    fn adapter_receives_only_metadata_and_failure_is_nonfatal() {
        struct Recording(RefCell<Vec<String>>);
        impl NotificationAdapter for Recording {
            fn show(&self, message: &NotificationMessage) -> Result<(), String> {
                self.0.borrow_mut().push(format!(
                    "{}:{}:{:?}",
                    message.host_label, message.session_name, message.attention
                ));
                Err("desktop unavailable".into())
            }
        }
        let recorder = Recording(RefCell::new(Vec::new()));
        deliver(
            &recorder,
            "local",
            vec![asd_client::attention::AttentionEffect {
                identity: asd_proto::SessionIdentity { instance_id: 1 },
                name: "agent".into(),
                kind: AttentionKind::Done,
            }],
        );
        assert_eq!(*recorder.0.borrow(), ["local:agent:Done"]);
    }
    #[test]
    fn untrusted_labels_are_bounded_and_plain() {
        assert_eq!(
            clean("<b>host</b>\x07\n\u{202e}&"),
            "&lt;b&gt;host&lt;/b&gt;&amp;"
        );
        assert_eq!(clean(&"a".repeat(1000)).len(), 160);
    }
}
