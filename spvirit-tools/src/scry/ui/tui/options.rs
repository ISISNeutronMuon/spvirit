use super::state::{FilterField, Page};

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OptionKind {
    Toggle,
    Text,
    Action,
    Section,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum OptionId {
    Pause,
    ShowHost,
    ResetCharts,
    EventFilter(usize),
    ConnView,
    ConnMidstream,
    ConnBroadcast,
    ConnSort,
    ConnLiveWindow,
    ConnShowStale,
    ConnSearch,
    ConnClearSearch,
    DecShowEndpoints,
    DecSort,
    DecQuickFilter,
    DecFilter(FilterField),
    DecClearFilters,
    FpView,
    FpShowPending,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub struct OptionItem {
    pub kind: OptionKind,
    pub id: OptionId,
}

const DASHBOARD_BASE: [OptionItem; 4] = [
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::Pause,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ShowHost,
    },
    OptionItem {
        kind: OptionKind::Action,
        id: OptionId::ResetCharts,
    },
    OptionItem {
        kind: OptionKind::Section,
        id: OptionId::ResetCharts,
    },
];

const CONNECTIONS_OPTIONS: [OptionItem; 8] = [
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ConnView,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ConnMidstream,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ConnBroadcast,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ConnSort,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ConnLiveWindow,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::ConnShowStale,
    },
    OptionItem {
        kind: OptionKind::Text,
        id: OptionId::ConnSearch,
    },
    OptionItem {
        kind: OptionKind::Action,
        id: OptionId::ConnClearSearch,
    },
];

const DECODED_OPTIONS: [OptionItem; 9] = [
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::DecShowEndpoints,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::DecSort,
    },
    OptionItem {
        kind: OptionKind::Action,
        id: OptionId::DecQuickFilter,
    },
    OptionItem {
        kind: OptionKind::Text,
        id: OptionId::DecFilter(FilterField::RequestType),
    },
    OptionItem {
        kind: OptionKind::Text,
        id: OptionId::DecFilter(FilterField::PvName),
    },
    OptionItem {
        kind: OptionKind::Text,
        id: OptionId::DecFilter(FilterField::SourceIp),
    },
    OptionItem {
        kind: OptionKind::Text,
        id: OptionId::DecFilter(FilterField::DestinationIp),
    },
    OptionItem {
        kind: OptionKind::Text,
        id: OptionId::DecFilter(FilterField::Text),
    },
    OptionItem {
        kind: OptionKind::Action,
        id: OptionId::DecClearFilters,
    },
];

const FINGERPRINTS_OPTIONS: [OptionItem; 2] = [
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::FpView,
    },
    OptionItem {
        kind: OptionKind::Toggle,
        id: OptionId::FpShowPending,
    },
];

pub fn options_for_page(page: Page, event_filter_len: usize) -> Vec<OptionItem> {
    match page {
        Page::Dashboard => {
            let mut items = DASHBOARD_BASE.to_vec();
            for idx in 0..event_filter_len {
                items.push(OptionItem {
                    kind: OptionKind::Toggle,
                    id: OptionId::EventFilter(idx),
                });
            }
            items
        }
        Page::Connections => CONNECTIONS_OPTIONS.to_vec(),
        Page::Decoded => {
            let mut items = DECODED_OPTIONS.to_vec();
            items.push(OptionItem {
                kind: OptionKind::Section,
                id: OptionId::ResetCharts,
            });
            for idx in 0..event_filter_len {
                items.push(OptionItem {
                    kind: OptionKind::Toggle,
                    id: OptionId::EventFilter(idx),
                });
            }
            items
        }
        Page::Diagnostics => Vec::new(),
        Page::Fingerprints => FINGERPRINTS_OPTIONS.to_vec(),
    }
}
