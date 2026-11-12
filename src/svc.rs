use crate::registry::{Kind, Registry, Service, State};

pub fn run(args: Vec<String>) -> i32 {
    let mut reg = Registry::new();
    reg.upsert(Service {
        id: "svc".into(),
        kind: Kind::Core,
        state: State::Live,
        deps: vec![],
        caps: vec!["list".into(), "describe".into()],
    });
    reg.upsert(Service {
        id: "wireguard".into(),
        kind: Kind::Plugin,
        state: State::Dead,
        deps: vec!["svc".into()],
        caps: vec!["status".into(), "up".into()],
    });

    match args.first().map(|s| s.as_str()) {
        Some("list") | None => {
            for id in reg.topo().unwrap_or_default() {
                println!("{}", id);
            }
            0
        }
        Some("describe") => {
            let id = args.get(1).cloned().unwrap_or_default();
            match reg.services.get(&id) {
                Some(s) => {
                    println!("{} {:?} {:?}", s.id, s.kind, s.state);
                    0
                }
                None => {
                    eprintln!("unknown service: {}", id);
                    2
                }
            }
        }
        _ => {
            eprintln!("usage: svc list | describe <id>");
            2
        }
    }
}
