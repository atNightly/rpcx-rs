use super::selector::ClientSelector;
use crossbeam::thread;
use etcd::{
    kv::{self, KeyValueInfo},
    Client,
};
use hyper::client::HttpConnector;
use std::{
    collections::HashMap,
    ops::Deref,
    sync::{Arc, RwLock},
};
use tokio::runtime::Runtime;

pub trait Discovery<'a> {
    fn get_services(&self) -> HashMap<String, String>;
    fn add_selector(&'a self, s: &'a (dyn ClientSelector + Sync + Send + 'static));
    fn close(&self);
}

#[derive(Default)]
pub struct StaticDiscovery<'a> {
    servers: HashMap<String, String>,
    selectors: Arc<RwLock<Vec<&'a (dyn ClientSelector + Sync + Send + 'static)>>>,
}

impl<'a> StaticDiscovery<'a> {
    pub fn new() -> StaticDiscovery<'a> {
        StaticDiscovery {
            servers: HashMap::new(),
            selectors: Arc::new(RwLock::new(Vec::new())),
        }
    }

    pub fn update_servers(&self, servers: &HashMap<String, String>) {
        let selectors = (*self).selectors.write().unwrap();
        let v = selectors.deref();
        for s in v {
            s.update_server(servers);
        }
    }
}

impl<'a> Discovery<'a> for StaticDiscovery<'a> {
    fn get_services(&self) -> HashMap<String, String> {
        let mut servers = HashMap::new();
        for (k, v) in &self.servers {
            servers.insert(k.clone(), v.clone());
        }
        servers
    }

    fn add_selector(&'a self, s: &'a (dyn ClientSelector + Sync + Send + 'static)) {
        let mut selectors = (*self).selectors.write().unwrap();
        selectors.push(s);
    }
    fn close(&self) {}
}

#[derive(Default)]
pub struct EtcdDiscovery<'a> {
    base_path: String,
    service_path: String,
    servers: Arc<RwLock<HashMap<String, String>>>,
    selectors: Arc<RwLock<Vec<&'a (dyn ClientSelector + Sync + Send + 'static)>>>,
}

impl<'a> EtcdDiscovery<'a> {
    pub fn new(
        client: Client<HttpConnector>,
        base_path: String,
        service_path: String,
    ) -> EtcdDiscovery<'a> {
        let mut d = EtcdDiscovery {
            base_path: base_path.clone(),
            service_path: service_path.clone(),
            servers: Arc::new(RwLock::new(HashMap::new())),
            selectors: Arc::new(RwLock::new(Vec::new())),
        };

        let selectors_cloned = d.selectors.clone();
        let servers_cloned = d.servers.clone();
        let mut prefix = base_path.clone();
        prefix.push('/');
        prefix.push_str(service_path.as_str());
        prefix.push('/');
        Self::list(&client, prefix.clone(), servers_cloned);

        let servers_cloned2 = d.servers.clone();

        // TODO: lifetime issue. can't run watch in a standalone thread

        // thread::scope(|s| {
        //     s.spawn(|_| {
        //         Self::watch(client, prefix, selectors_cloned, servers_cloned2);
        //     });
        // });

        // rayon::scope(|s| {
        //     s.spawn(|s| Self::watch(client, prefix, selectors_cloned, servers_cloned2));
        // });

        d
    }
    fn list(
        etc_client: &Client<HttpConnector>,
        prefix: String,
        servers: Arc<RwLock<HashMap<String, String>>>,
    ) {
        let key: String = prefix;

        let mut get_opt: kv::GetOptions = Default::default();
        get_opt.recursive = true;
        let op = kv::get(etc_client, key.as_str(), get_opt);

        match Runtime::new().unwrap().block_on(op) {
            Ok(resp) => {
                let kvi: KeyValueInfo = resp.data;
                if let Some(nodes) = kvi.node.nodes {
                    let mut m = servers.write().unwrap();
                    for node in &nodes {
                        if node.key.is_some() && node.value.is_some() {
                            let k = node.key.as_ref().unwrap().clone();
                            let v = node.value.as_ref().unwrap().clone();
                            let k2 = k.trim_start_matches(&key).to_owned();
                            m.insert(k2, v);
                        }
                    }
                }
            }
            Err(err) => eprintln!("{:?}", err),
        }
    }

    fn watch(
        etc_client: Client<HttpConnector>,
        prefix: String,
        selectors: Arc<RwLock<Vec<&'a (dyn ClientSelector + Sync + Send + 'static)>>>,
        servers: Arc<RwLock<HashMap<String, String>>>,
    ) {
        let key = prefix;
        let mut watch_opt: kv::WatchOptions = Default::default();
        watch_opt.recursive = true;
        loop {
            let changed = kv::watch(&etc_client, key.as_str(), watch_opt);
            match Runtime::new().unwrap().block_on(changed) {
                Ok(resp) => {
                    let kvi: KeyValueInfo = resp.data;
                    let node = kvi.node;
                    let k = node.key.as_ref().unwrap().clone();
                    let k2 = k.trim_start_matches(&key).to_owned();

                    let mut changed = false;
                    let mut m = servers.write().unwrap();
                    match kvi.action {
                        kv::Action::CompareAndDelete | kv::Action::Delete | kv::Action::Expire => {
                            m.remove(&k2);
                            changed = true;
                        }
                        _ => {
                            let mut v = "".to_owned();
                            if node.value.is_some() {
                                v = node.value.as_ref().unwrap().clone();
                            }
                            if m.contains_key(&k2) {
                                changed = *m.get(&k2).unwrap() != v
                            }
                            m.insert(k2, v);
                        }
                    }

                    if changed {
                        let selectors = selectors.write().unwrap();
                        let v = selectors.deref();
                        for s in v {
                            s.update_server(&*m);
                        }
                    }
                }
                Err(err) => eprintln!("{}", err),
            }
        }
    }

    pub fn update_servers(&self, servers: &HashMap<String, String>) {
        let selectors = (*self).selectors.write().unwrap();
        let v = selectors.deref();
        for s in v {
            s.update_server(servers);
        }
    }
}

impl<'a> Discovery<'a> for EtcdDiscovery<'a> {
    fn get_services(&self) -> HashMap<String, String> {
        let mut servers = HashMap::new();
        let ss = self.servers.read().unwrap();
        for (k, v) in &*ss {
            servers.insert(k.clone(), v.clone());
        }
        servers
    }

    fn add_selector(&'a self, s: &'a (dyn ClientSelector + Sync + Send + 'static)) {
        let mut selectors = (*self).selectors.write().unwrap();
        selectors.push(s);

        let ss = self.servers.read().unwrap();
        s.update_server(&*ss);
    }
    fn close(&self) {}
}
