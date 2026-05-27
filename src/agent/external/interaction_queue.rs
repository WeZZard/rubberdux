use std::collections::HashMap;
use std::sync::Mutex;

use tokio::sync::oneshot;

use super::{UIInteractionRequest, UIInteractionResponse};

pub struct PendingInteraction {
    pub request: UIInteractionRequest,
    pub response_tx: oneshot::Sender<UIInteractionResponse>,
}

pub struct InteractionQueue {
    pending: Mutex<HashMap<String, PendingInteraction>>,
}

impl InteractionQueue {
    pub fn new() -> Self {
        Self {
            pending: Mutex::new(HashMap::new()),
        }
    }

    pub fn add(&self, request_id: String, interaction: PendingInteraction) {
        self.pending.lock().unwrap().insert(request_id, interaction);
    }

    pub fn resolve(&self, request_id: &str, response: UIInteractionResponse) -> bool {
        if let Some(interaction) = self.pending.lock().unwrap().remove(request_id) {
            interaction.response_tx.send(response).is_ok()
        } else {
            false
        }
    }

    pub fn pending_count(&self) -> usize {
        self.pending.lock().unwrap().len()
    }

    pub fn pending_requests(&self) -> Vec<UIInteractionRequest> {
        self.pending
            .lock()
            .unwrap()
            .values()
            .map(|p| p.request.clone())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_add_and_resolve() {
        let queue = InteractionQueue::new();
        let (resp_tx, resp_rx) = oneshot::channel();
        let request = UIInteractionRequest::Question {
            request_id: "q-1".into(),
            agent_task_id: "task-1".into(),
            text: "Which?".into(),
            options: vec![],
        };
        queue.add(
            "q-1".into(),
            PendingInteraction {
                request,
                response_tx: resp_tx,
            },
        );
        assert_eq!(queue.pending_count(), 1);

        let resolved = queue.resolve(
            "q-1",
            UIInteractionResponse::SelectedOption {
                request_id: "q-1".into(),
                index: 0,
            },
        );
        assert!(resolved);
        assert_eq!(queue.pending_count(), 0);

        let response = resp_rx.await.unwrap();
        assert!(matches!(
            response,
            UIInteractionResponse::SelectedOption { index: 0, .. }
        ));
    }

    #[test]
    fn test_resolve_unknown() {
        let queue = InteractionQueue::new();
        let resolved = queue.resolve(
            "nonexistent",
            UIInteractionResponse::PlanApproved {
                request_id: "nonexistent".into(),
            },
        );
        assert!(!resolved);
    }

    #[tokio::test]
    async fn test_pending_count() {
        let queue = InteractionQueue::new();
        assert_eq!(queue.pending_count(), 0);

        let (tx1, _rx1) = oneshot::channel();
        let (tx2, _rx2) = oneshot::channel();
        queue.add(
            "q-1".into(),
            PendingInteraction {
                request: UIInteractionRequest::Question {
                    request_id: "q-1".into(),
                    agent_task_id: "".into(),
                    text: "".into(),
                    options: vec![],
                },
                response_tx: tx1,
            },
        );
        queue.add(
            "q-2".into(),
            PendingInteraction {
                request: UIInteractionRequest::Question {
                    request_id: "q-2".into(),
                    agent_task_id: "".into(),
                    text: "".into(),
                    options: vec![],
                },
                response_tx: tx2,
            },
        );
        assert_eq!(queue.pending_count(), 2);

        queue.resolve(
            "q-1",
            UIInteractionResponse::SelectedOption {
                request_id: "q-1".into(),
                index: 0,
            },
        );
        assert_eq!(queue.pending_count(), 1);
    }
}
