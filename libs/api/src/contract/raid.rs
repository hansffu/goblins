//! Retained raid record decoding and independent member display projection.
use super::*;
const RECORD_ORDER: &[&str] = &[
    "id",
    "name",
    "owner",
    "former_owner",
    "paused",
    "generation",
    "members",
];
const MEMBER_ORDER: &[&str] = &[
    "id",
    "agent_name",
    "name",
    "role",
    "state",
    "owner",
    "created_by",
    "options",
];
impl Response for RaidRecordReply {
    const SCHEMA: &'static str = "RaidRecordReply";
    fn decode(value: Value) -> io::Result<Self> {
        let mut value = ordered_value(value, RECORD_ORDER, 7).map_err(io::Error::other)?;
        if let Some(object) = value.as_object_mut() {
            object.entry("former_owner").or_insert(Value::Null);
            object.entry("paused").or_insert(Value::Bool(false));
            object.entry("generation").or_insert(serde_json::json!(0));
            if let Some(Value::Array(members)) = object.get_mut("members") {
                for member in members {
                    let mut normalized = ordered_value(std::mem::take(member), MEMBER_ORDER, 6)
                        .map_err(io::Error::other)?;
                    if let Some(object) = normalized.as_object_mut() {
                        object.entry("options").or_insert(serde_json::json!({}));
                    }
                    *member = normalized;
                }
            }
        }
        serde_json::from_value(value).map_err(io::Error::other)
    }
}
impl Response for RaidDisplayReply {
    const SCHEMA: &'static str = "RaidDisplayReply";
    fn decode(value: Value) -> io::Result<Self> {
        Ok(raid_display_reply(&value))
    }
}
pub fn raid_display_reply(record: &Value) -> RaidDisplayReply {
    let text = |value: &Value| value.as_str().unwrap_or("-").to_owned();
    RaidDisplayReply {
        id: text(&record["id"]),
        name: text(&record["name"]),
        former_owner: text(&record["former_owner"]),
        paused: record["paused"] == true,
        members: record["members"]
            .as_array()
            .into_iter()
            .flatten()
            .map(|member| RaidMemberDisplay {
                id: text(&member["id"]),
                agent_name: text(&member["agent_name"]),
                name: text(&member["name"]),
                role: text(&member["role"]),
                state: text(&member["state"]),
                owner: member["owner"] == true,
                created: member["created_by"].is_string(),
                options: crate::launch_choices(&member["options"])
                    .map_or("-".into(), |choices| choices.replace(", ", ",")),
            })
            .collect(),
    }
}

/// Decode the original session membership order and named defaults.
pub fn decode_session_raid(value: Value) -> io::Result<SessionRaid> {
    let value = ordered_value(value, &["id", "name", "role", "owner", "paused"], 4)
        .map_err(io::Error::other)?;
    serde_json::from_value::<SessionRaidReply>(value)
        .map(Into::into)
        .map_err(io::Error::other)
}
impl From<SessionRaidReply> for SessionRaid {
    fn from(r: SessionRaidReply) -> Self {
        Self {
            id: r.id,
            name: r.name,
            role: r.role,
            owner: r.owner,
            paused: r.paused,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn fixtures() -> Value {
        serde_json::from_str(include_str!("../../tests/fixtures.json")).unwrap()
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Invite {
        raid: String,
        sessions: Vec<String>,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Destroy {
        raid: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        generation: Option<u64>,
        #[serde(default)]
        stop_members: bool,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct SetOwner {
        raid: String,
        session: String,
        #[serde(default)]
        id: Option<String>,
        #[serde(default)]
        generation: Option<u64>,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Name {
        raid: String,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Kick {
        raid: String,
        session: String,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct HostRole {
        raid: String,
        session: String,
        role: String,
    }
    #[derive(Deserialize, Serialize)]
    struct OldMember {
        id: String,
        agent_name: String,
        name: String,
        role: Option<String>,
        state: String,
        owner: bool,
        #[serde(default)]
        created_by: Option<String>,
        #[serde(default)]
        options: std::collections::BTreeMap<String, String>,
    }
    #[derive(Deserialize, Serialize)]
    struct OldRecord {
        id: String,
        name: String,
        owner: Option<String>,
        #[serde(default)]
        former_owner: Option<String>,
        #[serde(default)]
        paused: bool,
        #[serde(default)]
        generation: u64,
        members: Vec<OldMember>,
    }
    #[test]
    fn host_raid_parameter_dtos_preserve_original_decoders_and_field_values() {
        for case in fixtures()["host_raid_parameter_cases"].as_array().unwrap() {
            let value = case["params"].clone();
            macro_rules! compare {
                ($old:ty,$method:ty) => {{
                    let old = serde_json::from_value::<$old>(value.clone());
                    let new = decode_params::<$method>(value.clone());
                    assert_eq!(old.is_ok(), new.is_ok(), "{case}");
                    let valid = new.is_ok();
                    if let (Ok(old), Ok(new)) = (old, new) {
                        let mut old = serde_json::to_value(old).unwrap();
                        for key in ["id", "generation"] {
                            if old.get(key) == Some(&Value::Null) {
                                old.as_object_mut().unwrap().remove(key);
                            }
                        }
                        assert_eq!(old, serde_json::to_value(new).unwrap(), "{case}");
                    }
                    valid
                }};
            }
            let valid = match case["method"].as_str().unwrap() {
                "raids.invite" => compare!(Invite, host::RaidsInvite),
                "raids.destroy" => compare!(Destroy, host::RaidsDestroy),
                "raids.set_owner" => compare!(SetOwner, host::RaidsSetOwner),
                "raids.members" => compare!(Name, host::RaidsMembers),
                "raids.kick" => compare!(Kick, host::RaidsKick),
                _ => compare!(HostRole, host::RaidsSetRole),
            };
            assert_eq!(valid, case["decoded"].as_bool().unwrap(), "{case}");
        }
    }
    #[test]
    fn raid_record_reader_matches_old_named_and_nested_positional_serde() {
        for case in fixtures()["raid_record_cases"].as_array().unwrap() {
            let old = serde_json::from_value::<OldRecord>(case["result"].clone());
            let new = RaidRecordReply::decode(case["result"].clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["legacy"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(new).unwrap(),
                    "{case}"
                );
            }
        }
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Role {
        role: String,
    }
    #[derive(Deserialize, Serialize)]
    #[serde(deny_unknown_fields)]
    struct Empty {}
    #[derive(Deserialize, Serialize)]
    struct OldMembership {
        id: String,
        name: String,
        role: Option<String>,
        owner: Option<String>,
        #[serde(default)]
        paused: bool,
    }
    #[test]
    fn sandbox_raid_parameters_preserve_original_decoders_and_positional_forms() {
        for case in fixtures()["sandbox_raid_parameter_cases"]
            .as_array()
            .unwrap()
        {
            let value = case["params"].clone();
            macro_rules! compare {
                ($old:ty,$method:ty) => {{
                    let old = serde_json::from_value::<$old>(value.clone());
                    let new = decode_params::<$method>(value.clone());
                    assert_eq!(old.is_ok(), new.is_ok(), "{case}");
                    assert_eq!(new.is_ok(), case["decoded"].as_bool().unwrap(), "{case}");
                    if let (Ok(old), Ok(new)) = (old, new) {
                        assert_eq!(
                            serde_json::to_value(old).unwrap(),
                            serde_json::to_value(new).unwrap(),
                            "{case}"
                        );
                    }
                }};
            }
            match case["method"].as_str().unwrap() {
                "raids.create" => compare!(Name, sandbox::RaidsCreate),
                "raids.set_role" => compare!(Role, sandbox::RaidsSetRole),
                "raids.leave" => compare!(Empty, sandbox::RaidsLeave),
                _ => compare!(Empty, sandbox::RaidsMembers),
            }
        }
    }
    #[test]
    fn session_membership_reader_preserves_original_serde_and_current_output() {
        for case in fixtures()["session_raid_cases"].as_array().unwrap() {
            let old = serde_json::from_value::<OldMembership>(case["result"].clone());
            let new = decode_session_raid(case["result"].clone());
            assert_eq!(old.is_ok(), new.is_ok(), "{case}");
            assert_eq!(new.is_ok(), case["legacy"].as_bool().unwrap(), "{case}");
            if let (Ok(old), Ok(new)) = (old, new) {
                assert_eq!(
                    serde_json::to_value(old).unwrap(),
                    serde_json::to_value(new).unwrap(),
                    "{case}"
                );
            }
        }
    }

    fn original_members(record: &serde_json::Value) -> String {
        let text = |v: &serde_json::Value| v.as_str().unwrap_or("-").to_string();
        let mut rows = vec![[
            "NAME".to_string(),
            "ROLE".into(),
            "CONFIGURATION".into(),
            "STATE".into(),
            "OWNER".into(),
            "CREATED".into(),
            "OPTIONS".into(),
            "ID".into(),
        ]];
        for m in record["members"].as_array().into_iter().flatten() {
            rows.push([
                text(&m["agent_name"]),
                text(&m["role"]),
                text(&m["name"]),
                text(&m["state"]),
                if m["owner"] == true { "owner" } else { "-" }.into(),
                // Run by an owner of the raid, which may stop it.
                if m["created_by"].is_string() {
                    "yes"
                } else {
                    "-"
                }
                .into(),
                crate::launch_choices(&m["options"]).map_or("-".into(), |c| c.replace(", ", ",")),
                text(&m["id"]),
            ]);
        }
        let widths: Vec<_> = (0..8)
            .map(|i| rows.iter().map(|r| r[i].chars().count()).max().unwrap_or(0))
            .collect();
        let mut out = format!("Raid {} ({})\n", text(&record["name"]), text(&record["id"]));
        if record["paused"] == true {
            out.push_str(&format!(
                "Paused: the owner {} left; waiting for the host to select a new owner\n",
                text(&record["former_owner"])
            ));
        }
        for row in rows {
            let line: Vec<_> = row
                .iter()
                .zip(&widths)
                .map(|(cell, width)| format!("{cell:width$}"))
                .collect();
            out.push_str(line.join("  ").trim_end());
            out.push('\n');
        }
        out
    }

    #[test]
    fn typed_raid_display_matches_complete_old_text_for_partial_and_malformed_records() {
        for record in fixtures()["raid_display_cases"].as_array().unwrap() {
            let expected = original_members(record);
            let typed = raid_display_reply(record);
            assert_eq!(crate::raid_members_typed(&typed), expected, "{record}");
            assert_eq!(crate::raid_members(record), expected, "{record}");
        }
    }
}
