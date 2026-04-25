use serde::{Deserialize, Serialize};

pub const TRAJECTORY_SCHEMA_VERSION: u32 = 1;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TrajectoryEvent {
    pub schema_version: u32,
    pub event_id: String,
    pub seq: u64,
    pub timestamp: String,
    #[serde(rename = "type")]
    pub event_type: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub agent_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub task_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub operation_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub caused_by: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
    #[serde(default, skip_serializing_if = "serde_json::Value::is_null")]
    pub payload: serde_json::Value,
}

#[derive(Debug, Clone)]
pub struct TrajectoryEventDraft {
    pub event_type: String,
    pub source: String,
    pub session_id: Option<String>,
    pub agent_id: String,
    pub turn_id: Option<String>,
    pub task_id: Option<String>,
    pub tool_call_id: Option<String>,
    pub operation_id: Option<String>,
    pub caused_by: Option<String>,
    pub actor: Option<String>,
    pub subject: Option<String>,
    pub payload: serde_json::Value,
}

impl TrajectoryEventDraft {
    pub fn new(
        event_type: impl Into<String>,
        source: impl Into<String>,
        agent_id: impl Into<String>,
    ) -> Self {
        Self {
            event_type: event_type.into(),
            source: source.into(),
            session_id: None,
            agent_id: agent_id.into(),
            turn_id: None,
            task_id: None,
            tool_call_id: None,
            operation_id: None,
            caused_by: None,
            actor: None,
            subject: None,
            payload: serde_json::Value::Null,
        }
    }

    pub fn with_session_id(mut self, session_id: Option<String>) -> Self {
        self.session_id = session_id;
        self
    }

    pub fn with_turn_id(mut self, turn_id: Option<String>) -> Self {
        self.turn_id = turn_id;
        self
    }

    pub fn with_task_id(mut self, task_id: Option<String>) -> Self {
        self.task_id = task_id;
        self
    }

    pub fn with_tool_call_id(mut self, tool_call_id: Option<String>) -> Self {
        self.tool_call_id = tool_call_id;
        self
    }

    pub fn with_operation_id(mut self, operation_id: Option<String>) -> Self {
        self.operation_id = operation_id;
        self
    }

    pub fn with_caused_by(mut self, caused_by: Option<String>) -> Self {
        self.caused_by = caused_by;
        self
    }

    pub fn with_actor(mut self, actor: impl Into<String>) -> Self {
        self.actor = Some(actor.into());
        self
    }

    pub fn with_subject(mut self, subject: impl Into<String>) -> Self {
        self.subject = Some(subject.into());
        self
    }

    pub fn with_payload(mut self, payload: serde_json::Value) -> Self {
        self.payload = payload;
        self
    }
}

impl TrajectoryEvent {
    pub fn from_draft(seq: u64, draft: TrajectoryEventDraft) -> Self {
        Self {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: format!("evt_{}", seq),
            seq,
            timestamp: chrono::Utc::now().to_rfc3339(),
            event_type: draft.event_type,
            source: draft.source,
            session_id: draft.session_id,
            agent_id: draft.agent_id,
            turn_id: draft.turn_id,
            task_id: draft.task_id,
            tool_call_id: draft.tool_call_id,
            operation_id: draft.operation_id,
            caused_by: draft.caused_by,
            actor: draft.actor,
            subject: draft.subject,
            payload: draft.payload,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn event_serializes_type_field() {
        let event = TrajectoryEvent::from_draft(
            7,
            TrajectoryEventDraft::new("agent.started", "test", "main")
                .with_payload(serde_json::json!({ "history_entries": 1 })),
        );

        let value = serde_json::to_value(event).unwrap();

        assert_eq!(value["schema_version"], TRAJECTORY_SCHEMA_VERSION);
        assert_eq!(value["event_id"], "evt_7");
        assert_eq!(value["type"], "agent.started");
        assert!(value.get("event_type").is_none());
        assert_eq!(value["payload"]["history_entries"], 1);
    }
}
