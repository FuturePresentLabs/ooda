//! GPC-1 implementation of OODA's bounded prediction interface.

use serde_json::{Map, Value, json};

use crate::HttpClient;
use crate::numeric::{
    BoundedError, BoundedPredictor, JointOutcome, JointRequest, NumericEstimate, NumericOutcome,
    NumericRequest, validate_joint_outcome, validate_joint_request, validate_numeric_outcome,
    validate_numeric_request,
};

/// GPC-1 over the same authenticated OpenAI-compatible gateway as [`HttpClient`].
///
/// Configure the wrapped client with the GPC-1 model slug exposed by the
/// gateway. Keeping this explicit avoids silently falling back to
/// `fpl/decide`, whose wire protocol and capabilities are different.
#[derive(Debug)]
pub struct Gpc1Client {
    http: HttpClient,
}

impl Gpc1Client {
    #[must_use]
    pub fn new(http: HttpClient) -> Self {
        Self { http }
    }

    fn request(&self, prompt: String, schema: Value, options: Value) -> Value {
        json!({
            "model": self.http.model(),
            "messages": [{"role": "user", "content": prompt}],
            "response_format": {
                "type": "json_schema",
                "json_schema": {"name": "result", "strict": true, "schema": schema}
            },
            "gpc1": options,
            "stream": false
        })
    }

    fn call(&self, body: &Value) -> Result<(Value, Option<String>, u128, u32), BoundedError> {
        let (value, resolved, elapsed, retries) = self
            .http
            .post(&self.http.url(crate::complete::CHAT_PATH), body)?;
        if let Some(message) = value
            .get("error")
            .and_then(|error| error.get("message").or(Some(error)))
            .and_then(Value::as_str)
        {
            return Err(BoundedError::InvalidResponse(message.to_owned()));
        }
        Ok((value, resolved, elapsed.as_millis(), retries))
    }
}

impl BoundedPredictor for Gpc1Client {
    fn estimate_numeric(&self, request: &NumericRequest) -> Result<NumericOutcome, BoundedError> {
        validate_numeric_request(request)?;
        let properties = request
            .fields
            .iter()
            .map(|field| {
                (
                    field.key.clone(),
                    json!({
                        "type": "number",
                        "minimum": field.minimum,
                        "maximum": field.maximum
                    }),
                )
            })
            .collect::<Map<_, _>>();
        let numeric_fields = request
            .fields
            .iter()
            .map(|field| serde_json::to_value(field).expect("NumericField is serializable"))
            .collect::<Vec<_>>();
        let schema = json!({
            "type": "object",
            "properties": properties,
            "required": request.fields.iter().map(|field| &field.key).collect::<Vec<_>>(),
            "additionalProperties": false
        });
        let prompt = format!(
            "{}\n\nObservation:\n{}",
            request.instruction, request.observation
        );
        let body = self.request(
            prompt,
            schema,
            json!({"mode": "numeric101", "numeric_fields": numeric_fields}),
        );
        let (raw, resolved_model, elapsed_ms, retries) = self.call(&body)?;
        let fields = raw
            .pointer("/gpc1/fields")
            .and_then(Value::as_object)
            .ok_or_else(|| BoundedError::InvalidResponse("missing gpc1.fields".into()))?;
        let estimates = fields
            .iter()
            .map(|(key, value)| {
                Ok(NumericEstimate {
                    key: key.clone(),
                    map_value: number(value, "map_value")?,
                    expected_value: number(value, "expected_value")?,
                    probabilities: numbers(value, "probabilities")?,
                    normalized_grid: optional_numbers(value, "normalized_grid")?,
                })
            })
            .collect::<Result<Vec<_>, BoundedError>>()?;
        let outcome = NumericOutcome {
            estimates,
            resolved_model,
            elapsed_ms: Some(elapsed_ms),
            retries: Some(retries),
            provider_metadata: raw.get("providerMetadata").cloned(),
        };
        validate_numeric_outcome(request, &outcome)?;
        Ok(outcome)
    }

    fn decide_joint(&self, request: &JointRequest) -> Result<JointOutcome, BoundedError> {
        validate_joint_request(request)?;
        let schema = joint_schema(&request.allowed_records)?;
        let prompt = format!(
            "{}\n\nObservation:\n{}",
            request.instruction, request.observation
        );
        let body = self.request(
            prompt,
            schema,
            json!({"mode": "finite_joint", "allowed_records": request.allowed_records}),
        );
        let (raw, resolved_model, elapsed_ms, retries) = self.call(&body)?;
        let gpc = raw
            .get("gpc1")
            .and_then(Value::as_object)
            .ok_or_else(|| BoundedError::InvalidResponse("missing gpc1 result".into()))?;
        let selected_index = gpc
            .get("selected_index")
            .and_then(Value::as_u64)
            .and_then(|value| usize::try_from(value).ok())
            .ok_or_else(|| BoundedError::InvalidResponse("missing selected_index".into()))?;
        let value = gpc
            .get("value")
            .and_then(Value::as_object)
            .cloned()
            .ok_or_else(|| BoundedError::InvalidResponse("missing selected value".into()))?;
        let outcome = JointOutcome {
            selected_index,
            value,
            probabilities: numbers_map(gpc, "probabilities")?,
            log_scores: optional_numbers_map(gpc, "log_scores")?,
            resolved_model,
            elapsed_ms: Some(elapsed_ms),
            retries: Some(retries),
            provider_metadata: raw.get("providerMetadata").cloned(),
        };
        validate_joint_outcome(request, &outcome)?;
        Ok(outcome)
    }
}

fn joint_schema(records: &[Map<String, Value>]) -> Result<Value, BoundedError> {
    let first = &records[0];
    let mut properties = Map::new();
    for (key, value) in first {
        let kind = match value {
            Value::String(_) => "string",
            Value::Bool(_) => "boolean",
            Value::Number(number) if number.is_i64() || number.is_u64() => "integer",
            Value::Number(_) => "number",
            Value::Null => "null",
            Value::Array(_) => "array",
            Value::Object(_) => "object",
        };
        if records.iter().any(|record| {
            record
                .get(key)
                .is_none_or(|candidate| json_type(candidate) != kind)
        }) {
            return Err(BoundedError::InvalidRequest(format!(
                "joint field {key:?} changes JSON type across records"
            )));
        }
        properties.insert(key.clone(), json!({"type": kind}));
    }
    Ok(json!({
        "type": "object",
        "properties": properties,
        "required": first.keys().collect::<Vec<_>>(),
        "additionalProperties": false
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

fn number(value: &Value, key: &str) -> Result<f64, BoundedError> {
    value
        .get(key)
        .and_then(Value::as_f64)
        .ok_or_else(|| BoundedError::InvalidResponse(format!("numeric field is missing {key}")))
}

fn numbers(value: &Value, key: &str) -> Result<Vec<f64>, BoundedError> {
    let object = value.as_object().ok_or_else(|| {
        BoundedError::InvalidResponse("numeric field result is not an object".into())
    })?;
    numbers_map(object, key)
}

fn optional_numbers(value: &Value, key: &str) -> Result<Vec<f64>, BoundedError> {
    let object = value.as_object().ok_or_else(|| {
        BoundedError::InvalidResponse("numeric field result is not an object".into())
    })?;
    optional_numbers_map(object, key)
}

fn numbers_map(object: &Map<String, Value>, key: &str) -> Result<Vec<f64>, BoundedError> {
    object
        .get(key)
        .and_then(Value::as_array)
        .ok_or_else(|| BoundedError::InvalidResponse(format!("missing {key}")))?
        .iter()
        .map(|value| {
            value.as_f64().ok_or_else(|| {
                BoundedError::InvalidResponse(format!("{key} contains a non-number"))
            })
        })
        .collect()
}

fn optional_numbers_map(object: &Map<String, Value>, key: &str) -> Result<Vec<f64>, BoundedError> {
    match object.get(key) {
        None => Ok(Vec::new()),
        Some(_) => numbers_map(object, key),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn joint_schema_refuses_type_drift() {
        let records = vec![
            Map::from_iter([("diameter".into(), json!(28.6))]),
            Map::from_iter([("diameter".into(), json!("28.6"))]),
        ];
        assert!(joint_schema(&records).is_err());
    }

    #[test]
    fn joint_schema_is_strict_and_requires_every_field() {
        let records = vec![
            Map::from_iter([
                ("diameter".into(), json!(28.6)),
                ("wall".into(), json!(0.9)),
            ]),
            Map::from_iter([
                ("diameter".into(), json!(31.8)),
                ("wall".into(), json!(0.8)),
            ]),
        ];
        let schema = joint_schema(&records).unwrap();
        assert_eq!(schema["additionalProperties"], false);
        assert_eq!(schema["properties"]["diameter"]["type"], "number");
        assert_eq!(schema["required"].as_array().unwrap().len(), 2);
    }
}
