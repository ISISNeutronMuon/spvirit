use std::sync::atomic::{AtomicUsize, Ordering};

#[derive(Clone, Debug)]
pub struct InterfaceInfo {
    pub name: String,
    pub description: String,
}

pub struct InterfaceSelection {
    pub interfaces: Vec<InterfaceInfo>,
    selected: AtomicUsize,
}

impl InterfaceSelection {
    pub fn new(interfaces: Vec<InterfaceInfo>, initial_index: usize) -> Self {
        let idx = if interfaces.is_empty() {
            0
        } else {
            initial_index.min(interfaces.len().saturating_sub(1))
        };
        Self {
            interfaces,
            selected: AtomicUsize::new(idx),
        }
    }

    pub fn len(&self) -> usize {
        self.interfaces.len()
    }

    pub fn current_index(&self) -> usize {
        let len = self.interfaces.len();
        if len == 0 {
            return 0;
        }
        let idx = self.selected.load(Ordering::Relaxed);
        idx.min(len.saturating_sub(1))
    }

    pub fn set_index(&self, idx: usize) {
        let len = self.interfaces.len();
        if len == 0 {
            return;
        }
        let clamped = idx.min(len.saturating_sub(1));
        self.selected.store(clamped, Ordering::Relaxed);
    }

    pub fn current_name(&self) -> Option<String> {
        let idx = self.current_index();
        self.interfaces.get(idx).map(|iface| iface.name.clone())
    }

    pub fn cycle_next(&self) -> usize {
        let len = self.interfaces.len();
        if len == 0 {
            return 0;
        }
        let next = (self.current_index() + 1) % len;
        self.selected.store(next, Ordering::Relaxed);
        next
    }
}

pub fn default_interface_index(interfaces: &[InterfaceInfo]) -> usize {
    for (idx, iface) in interfaces.iter().enumerate() {
        let name = iface.name.to_lowercase();
        let desc = iface.description.to_lowercase();
        let is_loopback = name == "lo"
            || name.starts_with("lo")
            || desc.contains("loopback")
            || desc.contains("loop back");
        if !is_loopback {
            return idx;
        }
    }
    0
}

pub fn find_interface_index(interfaces: &[InterfaceInfo], name: &str) -> Option<usize> {
    interfaces
        .iter()
        .position(|iface| iface.name == name)
}
