//! Which goblin an exec runs in.
use super::*;

#[test]
fn a_pin_never_reaches_a_namesake() {
    let _no_forks = unix::no_forks();
    let mut d = daemon();
    let path = d.state.clone();
    for (id, name) in [("old", "gribble"), ("other", "snaga")] {
        live(&mut d, id);
        d.sessions.get_mut(id).unwrap().record.agent_name = name.into();
    }
    let refused = |message: &str| Err((-32009, message.to_string()));
    // A pin names one session, by its ID or its name.
    assert_eq!(d.exec_target("gribble", Some("old")), Ok("old".into()));
    assert_eq!(d.exec_target("old", Some("old")), Ok("old".into()));
    assert_eq!(
        d.exec_target("snaga", Some("old")),
        refused("goblin snaga is not session old")
    );
    // The same refusal on the wire, for the pin a host exec sends.
    let (mut c, _peer) = connection(Role::Host);
    c.initialized = true;
    c.fds = (0..3).map(|_| unix::pipe().unwrap().0).collect();
    let reply = dispatch(
        &mut d,
        &mut c,
        "sessions.exec",
        json!({"session":"snaga","pin":"old","argv":["true"],"tty":false,"interactive":false}),
        Some(json!(1)),
    );
    assert_eq!(
        reply["error"],
        json!({"code":-32009,"message":"goblin snaga is not session old"})
    );
    // The pinned goblin stops; a new one takes its name.
    let old = d.sessions.get_mut("old").unwrap();
    old.record.state = "stopped".into();
    old.runtime = None;
    assert_eq!(
        d.exec_target("gribble", Some("old")),
        refused("goblin gribble (old) has stopped")
    );
    live(&mut d, "new");
    d.sessions.get_mut("new").unwrap().record.agent_name = "gribble".into();
    assert_eq!(
        d.exec_target("gribble", Some("old")),
        refused("goblin gribble (old) has stopped")
    );
    // Unpinned, the name means the new goblin.
    assert_eq!(d.exec_target("gribble", None), Ok("new".into()));
    // A pin this daemon never ran.
    assert_eq!(
        d.exec_target("gribble", Some("elsewhere")),
        refused("goblin gribble (elsewhere) has stopped or is not on this daemon")
    );
    drop(d);
    fs::remove_dir_all(path).unwrap();
}
