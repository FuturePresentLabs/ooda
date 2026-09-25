//! Strict-schema LLM fallback for bounded prediction.
//!
//! The fallback receives the same numerical bounds or finite record set as
//! GPC-1. It may fill the schema, but it cannot widen the domain. Unlike
//! GPC-1, it returns no calibrated support distribution; empty probability
//! arrays make that absence explicit.

use serde_json::{Map, Value, json};

use crate::HttpClient;
use crate::numeric::{
    BoundedError, BoundedPredictor, JointOutcome, JointRequest, NumericEstimate, NumericOutcome,
    NumericRequest, validate_joint_outcome, validate_joint_request, validate_numeric_outcome,
    validate_numeric_request,
};

#[derive(Debug)]
pub struct JsonSchemaPredictor {
    http: HttpClient,
}

impl JsonSchemaPredictor {
    #[must_use]
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    fn call(
        &self,
        prompt: String,
        schema: Value,
    ) -> Result<(Map<String, Value>, Option<String>, u128, u32, Option<Value>), BoundedError> {
        let body = json!({
            "model": self.http.model(),
            "messages": [
                {"role": "system", "content": "Return only a value satisfying the supplied strict JSON schema. Do not invent fields or values outside its bounds."},
                {"role": "user", "content": prompt}
            ],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "result", "strict": true, "schema": schema}
            },
            "temperature": 0,
            "stream": false
        });
        let (raw, resolved, elapsed, retries) = self
            .http
            .post(&self.http.url(crate::complete::CHAT_PATH), &body)?;
        let content = raw
            .pointer("/choices/0/message/content")
            .and_then(Value::as_str)
            .ok_or_else(|| {
                BoundedError::InvalidResponse("fallback returned no message content".into())
            })?;
        let value: Value = serde_json::from_str(content).map_err(|error| {
            BoundedError::InvalidResponse(format!("fallback content was not JSON: {error}"))
        })?;
        let object = value.as_object().cloned().ok_or_else(|| {
            BoundedError::InvalidResponse("fallback content was not a JSON object".into())
        })?;
        Ok((
            object,
            resolved,
            elapsed.as_millis(),
            retries,
            raw.get("providerMetadata").cloned(),
        ))
    }
}

impl BoundedPredictor for JsonSchemaPredictor {
    fn estimate_numeric(&self, request: &NumericRequest) -> Result<NumericOutcome, BoundedError> {
        validate_numeric_request(request)?;
        let properties = request
            .fields
            .iter()
            .map(|field| {
                (
                    field.key.clone(),
                    json!({"type":"number", "minimum":field.minimum, "maximum":field.maximum}),
                )
            })
            .collect::<Map<_, _>>();
        let schema = json!({
            "type":"object",
            "properties":properties,
            "required":request.fields.iter().map(|field| &field.key).collect::<Vec<_>>(),
            "additionalProperties":false
        });
        let prompt = format!(
            "{}\n\nObservation:\n{}",
            request.instruction, request.observation
        );
        let (values, resolved_model, elapsed_ms, retries, provider_metadata) =
            self.call(prompt, schema)?;
        let estimates = request
            .fields
            .iter()
            .map(|field| {
                let value = values
                    .get(&field.key)
                    .and_then(Value::as_f64)
                    .ok_or_else(|| {
                        BoundedError::InvalidResponse(format!(
                            "fallback omitted numeric field {:?}",
                            field.key
                        ))
                    })?;
                Ok(NumericEstimate {
                    key: field.key.clone(),
                    map_value: value,
                    expected_value: value,
                    probabilities: vec![],
                    normalized_grid: vec![],
                })
            })
            .collect::<Result<Vec<_>, BoundedError>>()?;
        let outcome = NumericOutcome {
            estimates,
            resolved_model,
            elapsed_ms: Some(elapsed_ms),
            retries: Some(retries),
            provider_metadata,
        };
        validate_numeric_outcome(request, &outcome)?;
        Ok(outcome)
    }

    fn decide_joint(&self, request: &JointRequest) -> Result<JointOutcome, BoundedError> {
        validate_joint_request(request)?;
        let schema = finite_record_schema(&request.allowed_records)?;
        let prompt = format!(
            "{}\nChoose exactly one of these complete records:\n{}\n\nObservation:\n{}",
            request.instruction,
            serde_json::to_string(&request.allowed_records).expect("records are JSON"),
            request.observation
        );
        let (value, resolved_model, elapsed_ms, retries, provider_metadata) =
            self.call(prompt, schema)?;
        let selected_index = request
            .allowed_records
            .iter()
            .position(|record| record == &value)
            .ok_or_else(|| {
                BoundedError::InvalidResponse(
                    "fallback returned a record outside the finite allowed set".into(),
                )
            })?;
        let outcome = JointOutcome {
            selected_index,
            value,
            probabilities: vec![],
            log_scores: vec![],
            resolved_model,
            elapsed_ms: Some(elapsed_ms),
            retries: Some(retries),
            provider_metadata,
        };
        validate_joint_outcome(request, &outcome)?;
        Ok(outcome)
    }
}

/// Primary bounded predictor with a schema-identical fallback.
#[derive(Debug)]
pub struct WithFallback<P, F> {
    pub primary: P,
    pub fallback: F,
}

impl<P, F> WithFallback<P, F> {
    #[must_use]
    pub fn new(primary: P, fallback: F) -> Self {
        Self { primary, fallback }
    }
}

impl<P: BoundedPredictor, F: BoundedPredictor> BoundedPredictor for WithFallback<P, F> {
    fn estimate_numeric(&self, request: &NumericRequest) -> Result<NumericOutcome, BoundedError> {
        match self.primary.estimate_numeric(request) {
            Ok(outcome) => Ok(outcome),
            Err(primary_error) => {
                let mut outcome = self.fallback.estimate_numeric(request)?;
                record_fallback(&mut outcome.provider_metadata, &primary_error);
                Ok(outcome)
            }
        }
    }

    fn decide_joint(&self, request: &JointRequest) -> Result<JointOutcome, BoundedError> {
        match self.primary.decide_joint(request) {
            Ok(outcome) => Ok(outcome),
            Err(primary_error) => {
                let mut outcome = self.fallback.decide_joint(request)?;
                record_fallback(&mut outcome.provider_metadata, &primary_error);
                Ok(outcome)
            }
        }
    }
}

fn record_fallback(metadata: &mut Option<Value>, error: &BoundedError) {
    let previous = metadata.take();
    *metadata = Some(json!({
        "ooda": {
            "fallback_used": true,
            "primary_error": error.to_string(),
            "calibrated_distribution": false
        },
        "provider": previous
    }));
}

fn finite_record_schema(records: &[Map<String, Value>]) -> Result<Value, BoundedError> {
    let first = &records[0];
    let mut properties = Map::new();
    for (key, value) in first {
        let kind = json_type(value);
        if records
            .iter()
            .any(|record| record.get(key).map(json_type) != Some(kind))
        {
            return Err(BoundedError::InvalidRequest(format!(
                "joint field {key:?} changes JSON type across records"
            )));
        }
        properties.insert(key.clone(), json!({"type":kind}));
    }
    Ok(json!({
        "type":"object",
        "properties":properties,
        "required":first.keys().collect::<Vec<_>>(),
        "additionalProperties":false
    }))
}

fn json_type(value: &Value) -> &'static str {
    match value {
        Value::String(_) => "string",
        Value::Bool(_) => "boolean",
        Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
        Value::Number(_) => "number",
        Value::Null => "null",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Fails;
    impl BoundedPredictor for Fails {
        fn estimate_numeric(&self, _: &NumericRequest) -> Result<NumericOutcome, BoundedError> {
            Err(BoundedError::InvalidResponse("primary unavailable".into()))
        }
        fn decide_joint(&self, _: &JointRequest) -> Result<JointOutcome, BoundedError> {
            Err(BoundedError::InvalidResponse("primary unavailable".into()))
        }
    }

    struct NumericFallback;
    impl BoundedPredictor for NumericFallback {
        fn estimate_numeric(
            &self,
            request: &NumericRequest,
        ) -> Result<NumericOutcome, BoundedError> {
            Ok(NumericOutcome {
                estimates: request
                    .fields
                    .iter()
                    .map(|field| NumericEstimate {
                        key: field.key.clone(),
                        map_value: field.minimum,
                        expected_value: field.minimum,
                        probabilities: vec![],
                        normalized_grid: vec![],
                    })
                    .collect(),
                resolved_model: Some("fallback".into()),
                elapsed_ms: None,
                retries: None,
                provider_metadata: None,
            })
        }
        fn decide_joint(&self, request: &JointRequest) -> Result<JointOutcome, BoundedError> {
            Ok(JointOutcome {
                selected_index: 0,
                value: request.allowed_records[0].clone(),
                probabilities: vec![],
                log_scores: vec![],
                resolved_model: Some("fallback".into()),
                elapsed_ms: None,
                retries: None,
                provider_metadata: None,
            })
        }
    }

    #[test]
    fn fallback_is_explicit_and_has_no_fake_distribution() {
        let predictor = WithFallback::new(Fails, NumericFallback);
        let request = NumericRequest {
            observation: Value::Null,
            instruction: "estimate".into(),
            fields: vec![crate::NumericField {
                key: "x".into(),
                description: "x".into(),
                minimum: 0.0,
                maximum: 1.0,
                unit: "ratio".into(),
                reference: None,
                aliases: vec![],
            }],
            correlation: None,
        };
        let outcome = predictor.estimate_numeric(&request).unwrap();
        assert!(outcome.estimates[0].probabilities.is_empty());
        assert_eq!(
            outcome.provider_metadata.unwrap()["ooda"]["fallback_used"],
            true
        );
    }
}
