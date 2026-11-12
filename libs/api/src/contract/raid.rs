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
