//! Bounded numerical and dependent-record decisions.
//!
//! These are the continuous and joint counterparts to [`crate::Question::Choice`].
//! The caller owns the domain: units, finite bounds, reference frames, and the
//! complete allowed records. A model may score only that supplied support;
//! callers still calculate values that are mathematically determined and
//! validate every estimate before acting on it.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NumericField {
    pub key: String,
    pub description: String,
    pub minimum: f64,
    pub maximum: f64,
    pub unit: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reference: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NumericRequest {
    pub observation: serde_json::Value,
    pub instruction: String,
    pub fields: Vec<NumericField>,
    pub correlation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NumericEstimate {
    pub key: String,
    pub map_value: f64,
    pub expected_value: f64,
    pub probabilities: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub normalized_grid: Vec<f64>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct NumericOutcome {
    pub estimates: Vec<NumericEstimate>,
    pub resolved_model: Option<String>,
    pub elapsed_ms: Option<u128>,
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<serde_json::Value>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JointRequest {
    pub observation: serde_json::Value,
    pub instruction: String,
    /// Complete, already-valid records. The model cannot combine fields from
    /// different rows or return a record not present here.
    pub allowed_records: Vec<serde_json::Map<String, serde_json::Value>>,
    pub correlation: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct JointOutcome {
    pub selected_index: usize,
    pub value: serde_json::Map<String, serde_json::Value>,
    pub probabilities: Vec<f64>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub log_scores: Vec<f64>,
    pub resolved_model: Option<String>,
    pub elapsed_ms: Option<u128>,
    pub retries: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub provider_metadata: Option<serde_json::Value>,
}

#[derive(Debug, thiserror::Error)]
pub enum BoundedError {
    #[error("invalid bounded decision request: {0}")]
    InvalidRequest(String),
    #[error("bounded decision response violated its supplied domain: {0}")]
    InvalidResponse(String),
    #[error(transparent)]
    Transport(#[from] crate::Error),
}

pub trait BoundedPredictor {
    fn estimate_numeric(&self, request: &NumericRequest) -> Result<NumericOutcome, BoundedError>;
    fn decide_joint(&self, request: &JointRequest) -> Result<JointOutcome, BoundedError>;
}

pub(crate) fn validate_numeric_request(request: &NumericRequest) -> Result<(), BoundedError> {
    if request.instruction.trim().is_empty() || request.fields.is_empty() {
        return Err(BoundedError::InvalidRequest(
            "numeric request needs an instruction and at least one field".into(),
        ));
    }
    let mut keys = BTreeSet::new();
    for field in &request.fields {
        if field.key.trim().is_empty()
            || field.description.trim().is_empty()
            || field.unit.trim().is_empty()
            || !keys.insert(field.key.as_str())
        {
            return Err(BoundedError::InvalidRequest(
                "numeric fields need unique non-empty keys, descriptions, and units".into(),
            ));
        }
        if !field.minimum.is_finite()
            || !field.maximum.is_finite()
            || field.minimum >= field.maximum
        {
            return Err(BoundedError::InvalidRequest(format!(
                "field {:?} needs finite ascending bounds",
                field.key
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_numeric_outcome(
    request: &NumericRequest,
    outcome: &NumericOutcome,
) -> Result<(), BoundedError> {
    let fields = request
        .fields
        .iter()
        .map(|field| (field.key.as_str(), field))
        .collect::<BTreeMap<_, _>>();
    if outcome.estimates.len() != fields.len() {
        return Err(BoundedError::InvalidResponse(format!(
            "received {} estimates for {} fields",
            outcome.estimates.len(),
            fields.len()
        )));
    }
    let mut seen = BTreeSet::new();
    for estimate in &outcome.estimates {
        let field = fields.get(estimate.key.as_str()).ok_or_else(|| {
            BoundedError::InvalidResponse(format!("unoffered field {:?}", estimate.key))
        })?;
        if !seen.insert(estimate.key.as_str()) {
            return Err(BoundedError::InvalidResponse(format!(
                "repeated field {:?}",
                estimate.key
            )));
        }
        for (name, value) in [
            ("MAP", estimate.map_value),
            ("expected", estimate.expected_value),
        ] {
            if !value.is_finite() || !(field.minimum..=field.maximum).contains(&value) {
                return Err(BoundedError::InvalidResponse(format!(
                    "{name} value {value} for {:?} is outside [{}, {}] {}",
                    field.key, field.minimum, field.maximum, field.unit
                )));
            }
        }
        if (!estimate.probabilities.is_empty() && estimate.probabilities.len() != 101)
            || estimate
                .probabilities
                .iter()
                .any(|p| !p.is_finite() || *p < 0.0)
        {
            return Err(BoundedError::InvalidResponse(format!(
                "field {:?} needs a non-negative 101-point distribution",
                field.key
            )));
        }
    }
    Ok(())
}

pub(crate) fn validate_joint_request(request: &JointRequest) -> Result<(), BoundedError> {
    if request.instruction.trim().is_empty() || request.allowed_records.len() < 2 {
        return Err(BoundedError::InvalidRequest(
            "joint request needs an instruction and at least two records".into(),
        ));
    }
    let Some(first) = request.allowed_records.first() else {
        unreachable!()
    };
    if first.is_empty() {
        return Err(BoundedError::InvalidRequest(
            "joint records must not be empty".into(),
        ));
    }
    let keys = first.keys().collect::<Vec<_>>();
    if request
        .allowed_records
        .iter()
        .any(|record| record.keys().collect::<Vec<_>>() != keys)
    {
        return Err(BoundedError::InvalidRequest(
            "joint records must have identical fields".into(),
        ));
    }
    Ok(())
}

pub(crate) fn validate_joint_outcome(
    request: &JointRequest,
    outcome: &JointOutcome,
) -> Result<(), BoundedError> {
    if request.allowed_records.get(outcome.selected_index) != Some(&outcome.value) {
        return Err(BoundedError::InvalidResponse(
            "selected joint record was not the supplied row at selected_index".into(),
        ));
    }
    if (!outcome.probabilities.is_empty()
        && outcome.probabilities.len() != request.allowed_records.len())
        || outcome
            .probabilities
            .iter()
            .any(|p| !p.is_finite() || *p < 0.0)
    {
        return Err(BoundedError::InvalidResponse(
            "joint probabilities do not match the allowed record set".into(),
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::{Map, Value, json};

    fn request() -> NumericRequest {
        NumericRequest {
            observation: json!({"use": "commuter bicycle"}),
            instruction: "Estimate editable fit parameters.".into(),
            fields: vec![NumericField {
                key: "reach_mm".into(),
                description: "horizontal rider reach".into(),
                minimum: 360.0,
                maximum: 420.0,
                unit: "mm".into(),
                reference: Some("bottom bracket to head datum".into()),
                aliases: vec![],
            }],
            correlation: Some("bike-fit-1".into()),
        }
    }

    #[test]
    fn accepts_a_complete_in_range_distribution() {
        let request = request();
        let outcome = NumericOutcome {
            estimates: vec![NumericEstimate {
                key: "reach_mm".into(),
                map_value: 390.0,
                expected_value: 391.2,
                probabilities: vec![1.0 / 101.0; 101],
                normalized_grid: vec![],
            }],
            resolved_model: Some("harshatheg/GPC-1".into()),
            elapsed_ms: Some(12),
            retries: Some(0),
            provider_metadata: None,
        };
        validate_numeric_request(&request).unwrap();
        validate_numeric_outcome(&request, &outcome).unwrap();
    }

    #[test]
    fn rejects_an_estimate_outside_the_offered_domain() {
        let request = request();
        let outcome = NumericOutcome {
            estimates: vec![NumericEstimate {
                key: "reach_mm".into(),
                map_value: 999.0,
                expected_value: 390.0,
                probabilities: vec![1.0 / 101.0; 101],
                normalized_grid: vec![],
            }],
            resolved_model: None,
            elapsed_ms: None,
            retries: None,
            provider_metadata: None,
        };
        assert!(validate_numeric_outcome(&request, &outcome).is_err());
    }

    #[test]
    fn joint_result_must_be_one_exact_allowed_record() {
        let records = vec![
            Map::from_iter([("diameter_mm".into(), json!(28.6))]),
            Map::from_iter([("diameter_mm".into(), json!(31.8))]),
        ];
        let request = JointRequest {
            observation: Value::Null,
            instruction: "Choose stock.".into(),
            allowed_records: records.clone(),
            correlation: None,
        };
        let outcome = JointOutcome {
            selected_index: 1,
            value: records[0].clone(),
            probabilities: vec![0.2, 0.8],
            log_scores: vec![],
            resolved_model: None,
            elapsed_ms: None,
            retries: None,
            provider_metadata: None,
        };
        assert!(validate_joint_outcome(&request, &outcome).is_err());
    }
}
