//! Raid record decoding and independent member display projection.
use super::*;
impl Response for RaidRecordReply {
    const SCHEMA: &'static str = "RaidRecordReply";
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

/// Decode a session's raid membership record.
pub fn decode_session_raid(value: Value) -> io::Result<SessionRaid> {
    validate_result_shape(&value, "SessionRaidReply")?;
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
