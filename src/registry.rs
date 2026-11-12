use std::collections::BTreeMap;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Core,
    Plugin,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum State {
    Dead,
    Live,
}

pub struct Service {
    pub id: String,
    pub kind: Kind,
    pub state: State,
    pub deps: Vec<String>,
    pub caps: Vec<String>,
}

pub struct Registry {
    pub services: BTreeMap<String, Service>,
}

impl Registry {
    pub fn new() -> Self {
        Registry {
            services: BTreeMap::new(),
        }
    }

    pub fn upsert(&mut self, s: Service) {
        self.services.insert(s.id.clone(), s);
    }

    pub fn live(&self) -> Vec<&Service> {
        self.services
            .values()
            .filter(|s| matches!(s.state, State::Live))
            .collect()
    }

    pub fn topo(&self) -> Result<Vec<String>, String> {
        let mut indeg: BTreeMap<String, usize> = BTreeMap::new();
        let mut out: Vec<String> = Vec::new();
        for (id, svc) in &self.services {
            indeg.entry(id.clone()).or_insert(0);
            for d in &svc.deps {
                if !self.services.contains_key(d) {
                    return Err(format!("unknown dependency {} of {}", d, id));
                }
            }
        }
        for svc in self.services.values() {
            for d in &svc.deps {
                *indeg.entry(svc.id.clone()).or_insert(0) += 1;
                let _ = d;
            }
        }
        let mut ready: Vec<String> = indeg
            .iter()
            .filter(|(_, n)| **n == 0)
            .map(|(k, _)| k.clone())
            .collect();
        while let Some(id) = ready.pop() {
            out.push(id.clone());
            let deps: Vec<String> = self
                .services
                .values()
                .filter(|s| s.deps.contains(&id))
                .map(|s| s.id.clone())
                .collect();
            for d in deps {
                if let Some(n) = indeg.get_mut(&d) {
                    *n -= 1;
                    if *n == 0 {
                        ready.push(d);
                    }
                }
            }
        }
        if out.len() != self.services.len() {
            return Err("dependency cycle".into());
        }
        Ok(out)
    }
}
