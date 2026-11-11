//! Host-job result variants and the retained submission notification reader.
use super::*;
#[derive(Clone, Debug, Serialize)]
#[serde(untagged)]
pub enum HostJob {
    Active(HostJobActive),
    Completed(HostJobCompleted),
}
impl<'de> Deserialize<'de> for HostJob {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        let value = Value::deserialize(deserializer)?;
        let log = value
            .get("log")
            .and_then(Value::as_str)
            .and_then(|path| path.strip_prefix("/run/goblins/host-jobs/"))
            .and_then(|path| path.strip_suffix("/output.log"));
        if !log.is_some_and(|component| !component.is_empty() && !component.contains('/')) {
            return Err(serde::de::Error::custom("invalid host job log path"));
        }
        let completed = ["exit_code", "signal", "code"]
            .iter()
            .any(|key| value.get(*key).is_some());
        let result = if completed {
            validate_result_shape(&value, "HostJobCompleted").and_then(|()| {
                serde_json::from_value(value)
                    .map(Self::Completed)
                    .map_err(io::Error::other)
            })
        } else {
            validate_result_shape(&value, "HostJobActive").and_then(|()| {
                serde_json::from_value(value)
                    .map(Self::Active)
                    .map_err(io::Error::other)
            })
        };
        result.map_err(serde::de::Error::custom)
    }
}
pub fn host_submitted(frame: &Value) -> Option<HostSubmittedReply> {
    (frame.get("method").and_then(Value::as_str)
        == Some(sandbox_notifications::HostSubmitted::NAME))
    .then(|| HostSubmittedReply {
        request: frame
            .get("params")
            .and_then(|params| params.get("request"))
            .and_then(Value::as_str)
            .unwrap_or_default()
            .into(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../schema/fixtures.json")).unwrap()
    }
    #[test]
    fn job_request_decoders_match_old_structs_and_both_endpoints() {
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldJob {
            request: String,
        }
        #[derive(Deserialize)]
        #[serde(deny_unknown_fields)]
        struct OldEmpty {}
        for case in fixtures()["host_job_parameter_cases"].as_array().unwrap() {
            let value = case["params"].clone();
            let valid = match case["method"].as_str().unwrap() {
                "host.list" => {
                    let old = serde_json::from_value::<OldEmpty>(value.clone());
                    let a = decode_params::<host::HostList>(value.clone());
                    let b = decode_params::<sandbox::HostList>(value);
                    assert_eq!(old.is_ok(), a.is_ok());
                    assert_eq!(old.is_ok(), b.is_ok());
                    a.is_ok()
                }
                "host.status" => {
                    let old = serde_json::from_value::<OldJob>(value.clone());
                    let a = decode_params::<host::HostStatus>(value.clone());
                    let b = decode_params::<sandbox::HostStatus>(value);
                    assert_eq!(old.is_ok(), a.is_ok());
                    assert_eq!(old.is_ok(), b.is_ok());
                    if let (Ok(old), Ok(a), Ok(b)) = (&old, &a, &b) {
                        assert_eq!(old.request, a.request);
                        assert_eq!(old.request, b.request);
                    }
                    a.is_ok()
                }
                _ => {
                    let old = serde_json::from_value::<OldJob>(value.clone());
                    let a = decode_params::<host::HostCancel>(value.clone());
                    let b = decode_params::<sandbox::HostCancel>(value);
                    assert_eq!(old.is_ok(), a.is_ok());
                    assert_eq!(old.is_ok(), b.is_ok());
                    if let (Ok(old), Ok(a), Ok(b)) = (&old, &a, &b) {
                        assert_eq!(old.request, a.request);
                        assert_eq!(old.request, b.request);
                    }
                    a.is_ok()
                }
            };
            assert_eq!(valid, case["valid"].as_bool().unwrap(), "{case}");
        }
    }
    #[test]
    fn strict_job_records_match_complete_schema_corpus() {
        for case in fixtures()["host_job_result_cases"].as_array().unwrap() {
            let result = serde_json::from_value::<HostJob>(case["result"].clone());
            assert_eq!(result.is_ok(), case["current"].as_bool().unwrap(), "{case}");
            if let Ok(result) = result {
                assert_eq!(serde_json::to_value(result).unwrap(), case["result"]);
            }
        }
    }
    #[test]
    fn host_status_projection_and_ignored_results_keep_old_fallbacks() {
        for case in fixtures()["host_job_result_cases"]
            .as_array()
            .unwrap()
            .iter()
            .map(|case| case["result"].clone())
            .chain([
                serde_json::json!({"status":"finished","code":0.0,"message":7}),
                serde_json::json!({"status":"cancelled","code":9223372036854775808u64}),
            ])
        {
            let a = decode_reply::<host::HostStatus>(case.clone()).unwrap();
            let b = decode_reply::<sandbox::HostStatus>(case.clone()).unwrap();
            for reply in [a, b] {
                assert_eq!(
                    reply.status.as_deref(),
                    case.get("status").and_then(Value::as_str)
                );
                assert_eq!(
                    reply.request.as_deref(),
                    case.get("request").and_then(Value::as_str)
                );
                assert_eq!(
                    reply.message.as_deref(),
                    case.get("message").and_then(Value::as_str)
                );
                assert_eq!(reply.code, case.get("code").and_then(Value::as_i64));
            }
            assert!(decode_reply::<host::HostList>(case.clone()).is_ok());
            assert!(decode_reply::<sandbox::HostList>(case.clone()).is_ok());
            assert!(decode_reply::<host::HostCancel>(case.clone()).is_ok());
            assert!(decode_reply::<sandbox::HostCancel>(case).is_ok());
        }
        let job = HostJobParams {
            request: "not an identifier".into(),
        };
        let result = invoke::<sandbox::HostStatus, io::Error>(job, |name, params| {
            assert_eq!(name, "host.status");
            assert_eq!(params, serde_json::json!({"request":"not an identifier"}));
            Ok(serde_json::json!({"status":"running"}))
        })
        .unwrap();
        assert_eq!(result.status.as_deref(), Some("running"));
    }
    #[test]
    fn host_submission_reader_matches_method_only_legacy_classification() {
        for case in fixtures()["host_submitted_cases"].as_array().unwrap() {
            let frame = &case["frame"];
            let old =
                (frame.get("method") == Some(&Value::String("host.submitted".into()))).then(|| {
                    frame
                        .get("params")
                        .and_then(|params| params.get("request"))
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                });
            let new = host_submitted(frame);
            assert_eq!(
                new.as_ref().map(|reply| reply.request.as_str()),
                old,
                "{case}"
            );
            assert_eq!(
                new.as_ref().map(|reply| reply.request.as_str()),
                case["request"].as_str(),
                "{case}"
            );
        }
        assert_eq!(
            notification::<sandbox_notifications::HostSubmitted>(HostSubmittedParams {
                request: "fixture-job-1".into()
            })
            .unwrap(),
            fixtures()["host_submitted_cases"][0]["frame"]
        );
    }
}
