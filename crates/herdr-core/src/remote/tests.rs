use super::*;
use crate::events::{Filter, Sink};
use crate::paths::Paths;
use std::sync::atomic::AtomicBool;

#[test]
fn a_remote_missed_invalidates_unfiltered_document_readers_with_the_local_sequence() {
    let root = std::path::PathBuf::from(format!(
        "/var/tmp/hc-doc-missed-{}-{}",
        std::process::id(),
        crate::platform::random_hex()
    ));
    let paths = Paths {
        socket: root.join("core.sock"),
        holder_socket: root.join("holder.sock"),
        state: root.clone(),
        config: root.join("config.toml"),
    };
    paths.create_dirs().unwrap();
    let core = Core::start(paths, crate::api::CoreInfo { id: "here".into(), name: "here".into() }).unwrap();
    let machine = Machine {
        name: "rohan".into(),
        target: "rohan".into(),
        via: false,
        temporary: false,
        session: None,
        id: "remote".into(),
        host: "rohan".into(),
        version: "test".into(),
    };
    let (tx, mut rx) = mpsc::channel(4);
    let (filtered_tx, mut filtered_rx) = mpsc::channel(4);
    {
        let mut st = core.lock();
        st.events.seq = 7;
        st.machines.push(machine.clone());
        core.subscribe(&mut st, Filter::default(), Sink::Plain { tx, behind: Arc::new(AtomicBool::new(false)) });
        core.subscribe(
            &mut st,
            Filter { terminal: Some("t1".into()), ..Default::default() },
            Sink::Plain { tx: filtered_tx, behind: Arc::new(AtomicBool::new(false)) },
        );
    }
    let (frames, _rx) = mpsc::unbounded_channel();
    let line = Line::new(frames, &json!({}));
    mirror_events(&core, &machine, &line, b"{\"method\":\"missed\",\"params\":{\"seq\":9999,\"terminals\":[]}}\n");
    let msg: Value = serde_json::from_str(&rx.try_recv().expect("local missed notification")).unwrap();
    assert_eq!(msg["method"], "missed");
    assert_eq!(msg["params"]["seq"], 7);
    assert!(filtered_rx.try_recv().is_err());
    core.lock().machines.clear();
    mirror_event(&core, &machine, json!({"type": "doc", "data": {"name": "app", "version": 1}}));
    assert!(rx.try_recv().is_err(), "removed machines must not emit doc events");
    drop(core);
    std::fs::remove_dir_all(root).unwrap();
}

#[test]
fn a_line_to_type_never_goes_to_a_core_that_would_drop_it() {
    let line = |features: Value| Line::new(tokio::sync::mpsc::unbounded_channel().0, &json!({ "features": features }));
    let (old, new) = (line(json!(["shell"])), line(json!(["shell", "type"])));
    let typed = json!({ "argv": ["sh"], "type": "hx ." });
    let err = can_type(&old, "box", "terminal.run", &typed, "hc machine update box").unwrap_err();
    assert_eq!((err.kind.as_str(), err.hint.as_str()), ("needs_update", "hc machine update box"));
    assert!(err.message.contains("box's hc is older"), "{}", err.message);
    assert!(can_type(&new, "box", "terminal.run", &typed, "").is_ok());
    for plain in [json!({ "argv": ["sh"] }), json!({ "argv": ["sh"], "type": null })] {
        assert!(can_type(&old, "box", "terminal.run", &plain, "").is_ok(), "{plain}");
    }
}
