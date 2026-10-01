//! Delivery confidence survives client-only prediction/UI resets.
use super::reconnect_draft::{DraftTarget, MAX_TARGETS};

#[derive(Default)]
pub(super) struct ReconnectDelivery {
    unresolved: Vec<DraftTarget>,
    overflowed: bool,
}

impl ReconnectDelivery {
    pub fn sent(&mut self, target: &DraftTarget) {
        if self.unresolved.contains(target) {
            return;
        }
        if self.unresolved.len() == MAX_TARGETS {
            self.overflowed = true;
        } else {
            self.unresolved.push(target.clone());
        }
    }

    pub fn confirmed(&mut self, target: &DraftTarget) {
        self.unresolved.retain(|pending| pending != target);
    }

    pub fn is_clean(&self, target: &DraftTarget) -> bool {
        !self.overflowed && !self.unresolved.contains(target)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::client::endpoint::ClientEndpointId;

    fn target(id: usize) -> DraftTarget {
        DraftTarget {
            endpoint_id: ClientEndpointId::Local,
            pane_id: id.to_string(),
        }
    }

    #[test]
    fn confirmation_clears_only_its_original_target() {
        let mut delivery = ReconnectDelivery::default();
        assert!(delivery.is_clean(&target(0)));
        delivery.sent(&target(0));
        delivery.sent(&target(1));
        delivery.confirmed(&target(1));
        assert!(!delivery.is_clean(&target(0)));
        assert!(delivery.is_clean(&target(1)));
    }

    #[test]
    fn overflow_never_drops_unresolved_delivery_evidence() {
        let mut delivery = ReconnectDelivery::default();
        for id in 0..=MAX_TARGETS {
            delivery.sent(&target(id));
        }
        for id in 0..MAX_TARGETS {
            delivery.confirmed(&target(id));
        }
        assert!(!delivery.is_clean(&target(MAX_TARGETS)));
        assert!(!delivery.is_clean(&target(MAX_TARGETS + 1)));
        assert!(delivery.unresolved.is_empty());
    }
}
