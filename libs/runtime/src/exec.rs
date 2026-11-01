//! Pure exec validation against the canonical init wire contract.
pub use goblins_protocol::init::forwardable;
use goblins_protocol::init::{Command, MAX_MESSAGE};

/// Preserve the exact spawn encoding and byte limit before moving stdio.
/// `id` is the already allocated exec id, including its serialized digits.
pub fn validate_spawn(
    id: u64,
    argv: &Option<Vec<String>>,
    cwd: &Option<String>,
    env: &[(String, String)],
    tty: bool,
) -> Result<(), String> {
    let command = Command::Spawn {
        id,
        argv: argv.clone(),
        cwd: cwd.clone(),
        env: env.to_vec(),
        tty,
    };
    let bytes = serde_json::to_vec(&command).map_err(|e| e.to_string())?;
    if bytes.len() > MAX_MESSAGE {
        return Err("command too long".into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn canonical_spawn_wire_keeps_field_order_nulls_and_escaping() {
        let command = Command::Spawn {
            id: 12,
            argv: Some(vec!["é\n\"\\".into()]),
            cwd: None,
            env: vec![("A".into(), "b".into())],
            tty: false,
        };
        assert_eq!(
            serde_json::to_string(&command).unwrap(),
            r#"{"method":"spawn","params":{"id":12,"argv":["é\n\"\\"],"cwd":null,"env":[["A","b"]],"tty":false}}"#
        );
        assert_eq!(
            serde_json::to_string(&Command::Spawn {
                id: 1,
                argv: None,
                cwd: Some("/".into()),
                env: vec![],
                tty: true,
            })
            .unwrap(),
            r#"{"method":"spawn","params":{"id":1,"argv":null,"cwd":"/","env":[],"tty":true}}"#
        );
    }

    #[test]
    fn spawn_limit_counts_encoded_bytes_and_the_actual_exec_id() {
        for id in [9, 10, u64::MAX] {
            for tty in [false, true] {
                let mut argv = Some(vec!["é\n\"\\".to_owned()]);
                let cwd = Some("/é\"".into());
                let env = vec![("A".into(), "é\n".into())];
                let initial = serde_json::to_vec(&Command::Spawn {
                    id,
                    argv: argv.clone(),
                    cwd: cwd.clone(),
                    env: env.clone(),
                    tty,
                })
                .unwrap()
                .len();
                argv.as_mut().unwrap()[0].push_str(&"x".repeat(MAX_MESSAGE - initial - 1));
                assert!(validate_spawn(id, &argv, &cwd, &env, tty).is_ok());
                argv.as_mut().unwrap()[0].push('x');
                assert!(validate_spawn(id, &argv, &cwd, &env, tty).is_ok());
                // One more serialized id digit must tip an exact-limit request.
                if id == 9 {
                    assert_eq!(
                        validate_spawn(10, &argv, &cwd, &env, tty).unwrap_err(),
                        "command too long"
                    );
                }
                argv.as_mut().unwrap()[0].push('x');
                assert_eq!(
                    validate_spawn(id, &argv, &cwd, &env, tty).unwrap_err(),
                    "command too long"
                );
            }
        }
    }
}
