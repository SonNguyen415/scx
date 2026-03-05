use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::thread;

use futures::StreamExt;
use tarpc::{context::Context, server::{BaseChannel, Channel}};
use tarpc::tokio_serde::formats::Bincode;

use scx_orchestra::{Domain, Notification, NotifKind, ResourceKind, Orchestra, SOCKET_PATH};

struct GlobalState {
    available_cpus: Vec<u32>,
    domains:        HashMap<String, Domain>,
}

impl GlobalState {
    fn new() -> Self {
        GlobalState {
            available_cpus: (0..num_cpus::get() as u32).collect(),
            domains:        HashMap::new(),
        }
    }

    fn register(&mut self, domain: &str) -> Result<(), String> {
        if self.domains.contains_key(domain) {
            self.domains.get_mut(domain).unwrap().client_count += 1;
        } else {
            let mut dom = Domain::new(domain);
            dom.client_count = 1;
            self.domains.insert(domain.to_string(), dom);
            println!("[orchestra] registered '{}'", domain);
        }
        Ok(())
    }

    fn unregister(&mut self, domain: &str) -> Result<(), String> {
        let dom = self.domains.get_mut(domain).ok_or("domain not found")?;
        dom.client_count -= 1;
        if dom.client_count == 0 {
            if let Some(dom) = self.domains.remove(domain) {
                self.available_cpus.extend(&dom.cpu_ids);
                println!("[orchestra] '{}' removed, reclaimed cpus={:?}", domain, dom.cpu_ids);
            }
        }
        Ok(())
    }

    fn request_cpu(&mut self, domain: &str, cpu_id: u32) -> Result<(), String> {
        if !self.available_cpus.contains(&cpu_id) {
            return Err(format!("cpu {} unavailable", cpu_id));
        }
        self.available_cpus.retain(|id| *id != cpu_id);
        let dom = self.domains.get_mut(domain).ok_or("domain not found")?;
        dom.cpu_ids.push(cpu_id);
        println!("[orchestra] '{}' cpus={:?}", domain, dom.cpu_ids);
        Ok(())
    }

    fn release_cpu(&mut self, domain: &str, cpu_id: u32) -> Result<(), String> {
        self.available_cpus.push(cpu_id);
        let dom = self.domains.get_mut(domain).ok_or("domain not found")?;
        dom.cpu_ids.retain(|id| *id != cpu_id);
        println!("[orchestra] '{}' released={} remaining={:?}", domain, cpu_id, dom.cpu_ids);
        Ok(())
    }

    fn cleanup(&mut self, domain: &str) {
        if let Some(dom) = self.domains.get_mut(domain) {
            dom.client_count -= 1;
            if dom.client_count == 0 {
                if let Some(dom) = self.domains.remove(domain) {
                    self.available_cpus.extend(&dom.cpu_ids);
                    println!("[orchestra] '{}' cleaned up, reclaimed cpus={:?}", domain, dom.cpu_ids);
                }
            }
        }
    }

    fn domain_expansion(&mut self, domain: &str, kind: ResourceKind, count: u32) -> u32 {
        match kind {
            ResourceKind::Cpu => {
                let mut fulfilled = 0;
                for _ in 0..count {
                    if let Some(cpu_id) = self.available_cpus.pop() {
                        let dom = self.domains.get_mut(domain).unwrap();
                        dom.cpu_ids.push(cpu_id);
                        dom.notif_queue.enqueue(Notification { kind: NotifKind::DomainExpansion, cpu_id });
                        fulfilled += 1;
                    } else { break; }
                }
                println!("[orchestra] expanded '{}' requested={} fulfilled={}", domain, count, fulfilled);
                fulfilled
            }
        }
    }

    fn domain_contraction(&mut self, domain: &str, kind: ResourceKind, count: u32) -> u32 {
        match kind {
            ResourceKind::Cpu => {
                let mut fulfilled = 0;
                let dom = self.domains.get_mut(domain).unwrap();
                for _ in 0..count {
                    if let Some(cpu_id) = dom.cpu_ids.pop() {
                        self.available_cpus.push(cpu_id);
                        dom.notif_queue.enqueue(Notification { kind: NotifKind::DomainContraction, cpu_id });
                        fulfilled += 1;
                    } else { break; }
                }
                println!("[orchestra] contracted '{}' requested={} fulfilled={}", domain, count, fulfilled);
                fulfilled
            }
        }
    }
}

type State = Arc<Mutex<GlobalState>>;

#[derive(Clone)]
struct OrchestraService(State);

impl Orchestra for OrchestraService {
    async fn register(self, _: Context, domain: String) -> Result<(), String> {
        self.0.lock().unwrap().register(&domain)
    }
    async fn unregister(self, _: Context, domain: String) -> Result<(), String> {
        self.0.lock().unwrap().unregister(&domain)
    }
    async fn request_cpu(self, _: Context, domain: String, cpu_id: u32) -> Result<(), String> {
        self.0.lock().unwrap().request_cpu(&domain, cpu_id)
    }
    async fn release_cpu(self, _: Context, domain: String, cpu_id: u32) -> Result<(), String> {
        self.0.lock().unwrap().release_cpu(&domain, cpu_id)
    }
}

fn run_poller(state: State) {
    loop {
        let st = state.lock().unwrap();
        for (name, domain) in st.domains.iter() {
            while let Some(task) = domain.dispatch_queue.dequeue() {
                println!("[orchestra] pid={} from '{}' nclients={}", task.pid, name, domain.client_count);
            }
        }
        drop(st);
        thread::sleep(std::time::Duration::from_millis(10));
    }
}

#[tokio::main]
async fn main() {
    if std::path::Path::new(SOCKET_PATH).exists() {
        std::fs::remove_file(SOCKET_PATH).unwrap();
    }

    let state: State = Arc::new(Mutex::new(GlobalState::new()));
    thread::spawn({ let s = Arc::clone(&state); move || run_poller(s) });

    let listener = tarpc::serde_transport::unix::listen(SOCKET_PATH, Bincode::default)
        .await.unwrap();

    println!("[orchestra] listening on {}", SOCKET_PATH);

    listener
        .filter_map(|r| async { r.ok() })
        .map(BaseChannel::with_defaults)
        .for_each(|ch| {
            let svc = OrchestraService(Arc::clone(&state));
            async move { ch.execute(svc.serve()).for_each(|f| async { f.await }).await }
        })
        .await;
}