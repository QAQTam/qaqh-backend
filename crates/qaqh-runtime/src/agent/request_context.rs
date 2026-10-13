use qaqh_gate::RequestContext;
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::session_fact_v2::FactPayload;

use super::state::agent::AgentState;
use super::tool_runtime::canonical_turn_id;

impl AgentState {
    /// Project committed task identity once per turn, then pass it explicitly to gate.
    pub(crate) fn model_request_context(
        &mut self,
        turn_id: &str,
        round_num: u32,
    ) -> Result<RequestContext, qaqh_gate::GateError> {
        let session_id = self.session.session_id.clone();
        let user_id = self.msg.turns().last().and_then(|turn| turn.user.msg_id);
        let key = format!("{session_id}:{turn_id}:{user_id:?}");
        if let Some((cached_key, context)) = &self.model_request_context
            && cached_key == &key
        {
            let mut context = context.clone();
            context.round_num = round_num;
            return Ok(context);
        }
        let mut context = RequestContext {
            session_id: session_id.clone(),
            request_set_id: key.clone(),
            round_num,
            turn_started_at: None,
        };
        if !self.ephemeral
            && let Some(sessions) = &self.session_manager
            && let Some(dir) = sessions
                .session_dir_for_id(&session_id)
                .map_err(qaqh_gate::GateError::msg)?
            && dir.join("events.commit.json").exists()
        {
            let identity = CanonicalSessionIdentity::open(&dir)?;
            if identity.session_id.as_str() != session_id {
                return Err(qaqh_gate::GateError::msg(
                    "model request session identity mismatch",
                ));
            }
            let reader = CommittedFactReader::open(&dir, identity.session_id, identity.log_id)?;
            let canonical_turn = canonical_turn_id(turn_id);
            for fact in reader.read_all()?.into_iter().rev() {
                if let FactPayload::TurnStarted(started) = fact.payload
                    && started.turn_id == canonical_turn
                {
                    context.request_set_id = started.input_id.as_str().to_owned();
                    context.turn_started_at = Some(fact.ts_ms);
                    break;
                }
            }
        }
        self.model_request_context = Some((key, context.clone()));
        Ok(context)
    }
}
