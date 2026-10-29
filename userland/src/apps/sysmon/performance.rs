use slopos_abi::net::{
    NET_IFKIND_WIRELESS, NET_OPER_DORMANT, NET_OPER_LOWERLAYERDOWN, NET_OPER_TESTING, NET_OPER_UP,
};
use slopos_abi::syscall::{
    BLOCK_STATS_READ_ONLY, BLOCK_STATS_REMOVABLE, BLOCK_TRANSPORT_NVME, BLOCK_TRANSPORT_USB,
    BLOCK_TRANSPORT_VIRTIO, DISPLAY_ADAPTER_FIRMWARE, DISPLAY_ADAPTER_INTEL_XE,
    DISPLAY_ADAPTER_VIRTIO_GPU,
};
use slopos_appkit::theme::space;
use slopos_appkit::theme::words::Bytes;
use slopos_appkit::{
    BarSegment, CrossAxisAlignment, Graph, GraphKey, List, ListRow, Node, Page, PropertyList,
    SIDEBAR_WIDE_WIDTH, Section, SegmentFill, Sidebar, Text, TextRole, Tone, column, nice_ceiling,
    row,
};
use slopos_cpufreq_core::CpuClass;
use slopos_installer_core::disk_size;
use slopos_net_core::{Ipv4, Mac};

use super::SysmonMsg;
use super::format::{count, cpu_features, duration, ghz, number, padded_text, percent, rate};
use super::monitor::{
    Adapter, DiskActivity, Display, History, WHOLE, WINDOW, free_bytes, total_bytes, used_bytes,
};
use super::state::{Resource, SysmonApp};

const CPU: usize = 0;
const DISK: usize = 1;
const MEMORY: usize = 4;
const GPU: usize = 5;
const NETWORK: usize = 6;

const FIGURE_WIDTH: i32 = 160;
const PROCESSOR_WIDTH: i32 = 120;
const FRAME_RATE_FLOOR: u64 = 60;
const THROUGHPUT_FLOOR: u64 = 1024;

struct Figure {
    label: &'static str,
    value: String,
    key: Option<GraphKey>,
}

fn figure(label: &'static str, value: impl Into<String>) -> Figure {
    Figure {
        label,
        value: value.into(),
        key: None,
    }
}

fn keyed(label: &'static str, value: impl Into<String>, key: GraphKey) -> Figure {
    Figure {
        label,
        value: value.into(),
        key: Some(key),
    }
}

fn caption(text: impl Into<String>) -> Text {
    Text::new(text)
        .role(TextRole::Caption)
        .tone(Tone::Secondary)
}

fn expand(child: Node<SysmonMsg>) -> Node<SysmonMsg> {
    Node::Expand {
        weight: 1,
        child: Box::new(child),
    }
}

fn plotted(what: &str, top: String, graph: Graph) -> Node<SysmonMsg> {
    column(
        space::XS,
        vec![
            row(
                space::SM,
                vec![expand(caption(what).into()), caption(top).into()],
            ),
            graph.into(),
            row(
                space::SM,
                vec![
                    expand(caption(format!("{WINDOW} seconds")).into()),
                    caption("0").into(),
                ],
            ),
        ],
    )
}

fn throughput_ceiling(histories: &[&History]) -> u64 {
    let peak = histories.iter().map(|h| h.peak()).max().unwrap_or(0);
    nice_ceiling(peak.max(THROUGHPUT_FLOOR), 1024)
}

fn adapter_kind(adapter: &Adapter) -> &'static str {
    match adapter.now.iface.kind {
        NET_IFKIND_WIRELESS => "Wi-Fi",
        _ => "Ethernet",
    }
}

fn connection(state: u8) -> &'static str {
    match state {
        NET_OPER_UP => "Connected",
        NET_OPER_LOWERLAYERDOWN => "No cable",
        NET_OPER_DORMANT => "Waiting to connect",
        NET_OPER_TESTING => "Testing",
        _ => "Not connected",
    }
}

fn transport(disk: &DiskActivity) -> &'static str {
    match disk.now.transport {
        BLOCK_TRANSPORT_NVME => "NVMe",
        BLOCK_TRANSPORT_USB => "USB",
        BLOCK_TRANSPORT_VIRTIO => "Virtio",
        _ => "Disk",
    }
}

fn display_adapter(display: &Display) -> &'static str {
    match display.now.adapter {
        DISPLAY_ADAPTER_VIRTIO_GPU => "Virtio GPU",
        DISPLAY_ADAPTER_INTEL_XE => "Intel graphics",
        DISPLAY_ADAPTER_FIRMWARE => "Basic display",
        _ => "Display",
    }
}

fn class_name(class: CpuClass) -> &'static str {
    match class {
        CpuClass::Performance => "performance core",
        CpuClass::Efficiency => "efficiency core",
    }
}

impl SysmonApp {
    pub(crate) fn view_performance(&self) -> Node<SysmonMsg> {
        let resources = self.resources();
        let rows = resources.iter().map(|r| self.sidebar_row(r)).collect();
        let chosen = resources.iter().position(|r| *r == self.resource);
        let sidebar = Sidebar::new(
            List::new(rows)
                .selected(chosen)
                .on_select(SysmonMsg::Resource),
        )
        .wide();
        Node::HStack {
            children: vec![sidebar.into(), expand(self.resource_page())],
            spacing: 0,
            align: CrossAxisAlignment::Stretch,
        }
    }

    fn graph(&self, label: &str) -> Graph {
        Graph::new(label, WINDOW).elapsed(self.monitor.intervals)
    }

    fn sidebar_row(&self, resource: &Resource) -> ListRow {
        let m = &self.monitor;
        match resource {
            Resource::Cpu => {
                let speed = match m.cpu.mhz {
                    Some(mhz) => ghz(mhz),
                    None => count(m.cpu.processors.len() as u64, "processor", "processors"),
                };
                ListRow::new("CPU")
                    .detail(format!("{}\n{speed}", percent(m.cpu.busy.last())))
                    .graph(self.graph("CPU").line(m.cpu.busy.values(), CPU).max(WHOLE))
            }
            Resource::Memory => {
                let (used, total) = (used_bytes(&m.now), total_bytes(&m.now));
                ListRow::new("Memory")
                    .detail(format!(
                        "{} of {}\n{} in use",
                        Bytes(used),
                        Bytes(total),
                        percent(share(used, total))
                    ))
                    .graph(
                        self.graph("Memory")
                            .line(m.memory.values(), MEMORY)
                            .max(total),
                    )
            }
            Resource::Disk(name) => {
                let Some(disk) = m.disks.iter().find(|d| d.name == *name) else {
                    return ListRow::new(format!("Disk ({name})"));
                };
                let model = disk.model.as_deref().unwrap_or(transport(disk));
                ListRow::new(format!("Disk ({name})"))
                    .detail(format!("{} active\n{model}", percent(disk.active.last())))
                    .graph(
                        self.graph("Disk")
                            .line(disk.active.values(), DISK)
                            .max(WHOLE),
                    )
            }
            Resource::Adapter(ifindex) => {
                let Some(adapter) = m.adapters.iter().find(|a| a.ifindex == *ifindex) else {
                    return ListRow::new("Network");
                };
                let ceiling = throughput_ceiling(&[&adapter.received, &adapter.sent]);
                ListRow::new(format!("{} ({})", adapter_kind(adapter), adapter.name))
                    .detail(format!(
                        "Receiving {}\nSending {}",
                        rate(adapter.received.last()),
                        rate(adapter.sent.last())
                    ))
                    .graph(
                        self.graph("Network")
                            .line(adapter.received.values(), NETWORK)
                            .dashed(adapter.sent.values(), NETWORK)
                            .max(ceiling),
                    )
            }
            Resource::Gpu => {
                let Some(display) = &m.display else {
                    return ListRow::new("GPU");
                };
                ListRow::new("GPU")
                    .detail(format!(
                        "{} busy\n{}",
                        percent(display.busy.last()),
                        display_adapter(display)
                    ))
                    .graph(
                        self.graph("GPU")
                            .line(display.busy.values(), GPU)
                            .max(WHOLE),
                    )
            }
        }
    }

    fn resource_page(&self) -> Node<SysmonMsg> {
        let m = &self.monitor;
        let page = match &self.resource {
            Resource::Cpu => Some(self.cpu_page()),
            Resource::Memory => Some(self.memory_page()),
            Resource::Disk(name) => m
                .disks
                .iter()
                .find(|d| d.name == *name)
                .map(|d| self.disk_page(d)),
            Resource::Adapter(ifindex) => m
                .adapters
                .iter()
                .find(|a| a.ifindex == *ifindex)
                .map(|a| self.adapter_page(a)),
            Resource::Gpu => m.display.as_ref().map(|d| self.display_page(d)),
        };
        page.unwrap_or_else(|| self.cpu_page())
    }

    fn content_width(&self) -> i32 {
        self.width - SIDEBAR_WIDE_WIDTH - 2 * space::XL
    }

    fn figures(&self, figures: Vec<Figure>) -> Node<SysmonMsg> {
        let per_row = (self.content_width() / FIGURE_WIDTH).clamp(2, 4) as usize;
        let rows = figures
            .chunks(per_row)
            .map(|chunk| {
                let mut cells: Vec<Node<SysmonMsg>> =
                    chunk.iter().map(|f| expand(figure_node(f))).collect();
                cells.resize_with(per_row, || expand(Node::Empty));
                Node::HStack {
                    children: cells,
                    spacing: space::LG,
                    align: CrossAxisAlignment::Start,
                }
            })
            .collect();
        column(space::LG, rows)
    }

    fn cpu_page(&self) -> Node<SysmonMsg> {
        let m = &self.monitor;
        let cpu = &m.cpu;
        let mut figures = vec![figure("Utilization", percent(cpu.busy.last()))];
        if let Some(mhz) = cpu.mhz {
            figures.push(figure("Speed", ghz(mhz)));
        }
        figures.extend([
            figure("Processes", number(m.process_count() as u64)),
            figure("Threads", number(m.thread_count() as u64)),
            figure("Up time", duration(m.now.at_ms)),
        ]);
        if let Some(celsius) = cpu.celsius {
            figures.push(figure("Temperature", format!("{celsius} °C")));
        }

        let info = &m.now.cpu_info;
        let mut about = PropertyList::new()
            .row("Vendor", padded_text(&info.vendor))
            .row(
                "Processors",
                count(cpu.processors.len() as u64, "processor", "processors"),
            );
        if m.now.perf_info.base_mhz != 0 {
            about = about.row("Base speed", ghz(u64::from(m.now.perf_info.base_mhz)));
        }
        if m.now.perf_info.max_mhz != 0 {
            about = about.row("Highest speed", ghz(u64::from(m.now.perf_info.max_mhz)));
        }
        about = about
            .row(
                "Family and model",
                format!(
                    "{} / {}, stepping {}",
                    info.family, info.model, info.stepping
                ),
            )
            .row("Features", cpu_features(info.features))
            .row(
                "Context switches",
                format!("{} a second", number(cpu.switches_per_second)),
            );

        let model = padded_text(&info.brand_string);
        Page::new("CPU")
            .subtitle(model)
            .content(column(
                space::XL,
                vec![
                    plotted(
                        "Utilization",
                        String::from("100%"),
                        self.graph("CPU utilization")
                            .line(cpu.busy.values(), CPU)
                            .max(WHOLE),
                    ),
                    self.figures(figures),
                    Section::new("Each processor", self.processors()).into(),
                    Section::new("About", about).into(),
                ],
            ))
            .into()
    }

    fn processors(&self) -> Node<SysmonMsg> {
        let count = self.monitor.cpu.processors.len().max(1);
        let per_row = ((self.content_width() / PROCESSOR_WIDTH).clamp(2, 8) as usize).min(count);
        let cells: Vec<Node<SysmonMsg>> = self
            .monitor
            .cpu
            .processors
            .iter()
            .map(|p| {
                let name = match p.class {
                    Some(class) => format!("CPU {}, {}", p.cpu, class_name(class)),
                    None => format!("CPU {}", p.cpu),
                };
                column(
                    space::XS,
                    vec![
                        row(
                            space::SM,
                            vec![
                                expand(caption(name).into()),
                                caption(percent(p.busy.last())).into(),
                            ],
                        ),
                        self.graph(&format!("CPU {}", p.cpu))
                            .line(p.busy.values(), CPU)
                            .max(WHOLE)
                            .small()
                            .into(),
                    ],
                )
            })
            .collect();
        let mut cells = cells.into_iter();
        let mut rows = Vec::new();
        loop {
            let mut chunk: Vec<Node<SysmonMsg>> =
                cells.by_ref().take(per_row).map(expand).collect();
            if chunk.is_empty() {
                break;
            }
            chunk.resize_with(per_row, || expand(Node::Empty));
            rows.push(Node::HStack {
                children: chunk,
                spacing: space::MD,
                align: CrossAxisAlignment::Start,
            });
        }
        column(space::MD, rows)
    }

    fn memory_page(&self) -> Node<SysmonMsg> {
        let m = &self.monitor;
        let sys = &m.now.sys;
        let (used, total, free) = (used_bytes(&m.now), total_bytes(&m.now), free_bytes(&m.now));
        let page = slopos_abi::PAGE_SIZE;
        let committed = u64::from(sys.committed_pages).saturating_mul(page);
        let limited = sys.commit_headroom_pages != u32::MAX;
        let limit = u64::from(sys.commit_limit_pages).saturating_mul(page);
        let figures = vec![
            figure("In use", Bytes(used).to_string()),
            figure("Free", Bytes(free).to_string()),
            figure("Committed", Bytes(committed).to_string()),
            figure("Apps ended for memory", number(u64::from(sys.oom_kills))),
        ];
        let composition = Node::PartitionBar {
            segments: vec![
                BarSegment {
                    label: String::from("In use"),
                    size: used,
                    fill: SegmentFill::Series(MEMORY),
                },
                BarSegment {
                    label: String::from("Free"),
                    size: free,
                    fill: SegmentFill::Hatched,
                },
            ],
            highlight: None,
        };
        let about = PropertyList::new()
            .row("Total", Bytes(total).to_string())
            .row("In use", Bytes(used).to_string())
            .row("Free", Bytes(free).to_string())
            .row("Committed", Bytes(committed).to_string())
            .row(
                "Commit limit",
                match limited {
                    true => Bytes(limit).to_string(),
                    false => String::from("None"),
                },
            )
            .row("Apps ended for memory", number(u64::from(sys.oom_kills)));
        Page::new("Memory")
            .subtitle(format!("{} in all", Bytes(total)))
            .content(column(
                space::XL,
                vec![
                    plotted(
                        "In use",
                        Bytes(total).to_string(),
                        self.graph("Memory in use")
                            .line(m.memory.values(), MEMORY)
                            .max(total),
                    ),
                    self.figures(figures),
                    Section::new("Composition", composition).into(),
                    Section::new("About", about).into(),
                ],
            ))
            .into()
    }

    fn disk_page(&self, disk: &DiskActivity) -> Node<SysmonMsg> {
        let now = &disk.now;
        let ceiling = throughput_ceiling(&[&disk.read, &disk.written]);
        let figures = vec![
            figure("Active time", percent(disk.active.last())),
            keyed("Reading", rate(disk.read.last()), GraphKey::new(DISK)),
            keyed(
                "Writing",
                rate(disk.written.last()),
                GraphKey::new(DISK).dashed(),
            ),
            figure("Capacity", disk_size(now.capacity_bytes)),
        ];
        let mut about = PropertyList::new()
            .row("Kind", transport(disk))
            .row("Capacity", disk_size(now.capacity_bytes))
            .row(
                "Read",
                format!(
                    "{} in {}",
                    Bytes(now.read_bytes),
                    count(now.read_requests, "request", "requests")
                ),
            )
            .row(
                "Written",
                format!(
                    "{} in {}",
                    Bytes(now.write_bytes),
                    count(now.write_requests, "request", "requests")
                ),
            )
            .row(
                "Block size",
                Bytes(u64::from(now.logical_block_size)).to_string(),
            );
        if let Some(model) = &disk.model {
            about = about.row("Model", model.clone());
        }
        if now.flags & BLOCK_STATS_REMOVABLE != 0 {
            about = about.row("Removable", "Yes");
        }
        if now.flags & BLOCK_STATS_READ_ONLY != 0 {
            about = about.row("Read-only", "Yes");
        }
        Page::new(format!("Disk ({})", disk.name))
            .subtitle(disk.model.clone().unwrap_or_else(|| transport(disk).into()))
            .content(column(
                space::XL,
                vec![
                    plotted(
                        "Active time",
                        String::from("100%"),
                        self.graph("Disk active time")
                            .line(disk.active.values(), DISK)
                            .max(WHOLE),
                    ),
                    plotted(
                        "Transfer rate",
                        rate(ceiling),
                        self.graph("Disk transfer rate")
                            .line(disk.read.values(), DISK)
                            .dashed(disk.written.values(), DISK)
                            .max(ceiling),
                    ),
                    self.figures(figures),
                    Section::new("About", about).into(),
                ],
            ))
            .into()
    }

    fn adapter_page(&self, adapter: &Adapter) -> Node<SysmonMsg> {
        let iface = &adapter.now.iface;
        let ceiling = throughput_ceiling(&[&adapter.received, &adapter.sent]);
        let status = connection(iface.oper_state);
        let figures = vec![
            keyed(
                "Receiving",
                rate(adapter.received.last()),
                GraphKey::new(NETWORK),
            ),
            keyed(
                "Sending",
                rate(adapter.sent.last()),
                GraphKey::new(NETWORK).dashed(),
            ),
            figure("Status", status),
        ];
        let errors = match (iface.rx_errors, iface.tx_errors) {
            (0, 0) => String::from("None"),
            (rx, tx) => format!("{} received, {} sent", number(rx), number(tx)),
        };
        let dropped = match (iface.rx_dropped, iface.tx_dropped) {
            (0, 0) => String::from("None"),
            (rx, tx) => format!("{} received, {} sent", number(rx), number(tx)),
        };
        let address = match adapter.now.addr {
            Some((addr, prefix)) => format!("{}/{prefix}", Ipv4(addr)),
            None => String::from("None"),
        };
        let about = PropertyList::new()
            .row("Status", status)
            .row("Address", address)
            .row("Hardware address", format!("{}", Mac(iface.mac)))
            .row("Largest packet", Bytes(u64::from(iface.mtu)).to_string())
            .row(
                "Received",
                format!(
                    "{} in {}",
                    Bytes(iface.rx_bytes),
                    count(iface.rx_packets, "packet", "packets")
                ),
            )
            .row(
                "Sent",
                format!(
                    "{} in {}",
                    Bytes(iface.tx_bytes),
                    count(iface.tx_packets, "packet", "packets")
                ),
            )
            .row("Errors", errors)
            .row("Dropped", dropped);
        Page::new(format!("{} ({})", adapter_kind(adapter), adapter.name))
            .subtitle(status)
            .content(column(
                space::XL,
                vec![
                    plotted(
                        "Throughput",
                        rate(ceiling),
                        self.graph("Network throughput")
                            .line(adapter.received.values(), NETWORK)
                            .dashed(adapter.sent.values(), NETWORK)
                            .max(ceiling),
                    ),
                    self.figures(figures),
                    Section::new("About", about).into(),
                ],
            ))
            .into()
    }

    fn display_page(&self, display: &Display) -> Node<SysmonMsg> {
        let now = &display.now;
        let frame_ceiling = nice_ceiling(display.frames.peak().max(FRAME_RATE_FLOOR), 1000);
        let resolution = format!("{} × {}", now.width, now.height);
        let figures = vec![
            figure("Busy", percent(display.busy.last())),
            figure("Frames per second", number(display.frames.last())),
            figure("Resolution", resolution.clone()),
            figure("Display memory", Bytes(now.scanout_bytes).to_string()),
        ];
        let about = PropertyList::new()
            .row("Adapter", display_adapter(display))
            .row("Resolution", resolution)
            .row("Display memory", Bytes(now.scanout_bytes).to_string())
            .row("Frames shown", number(now.frames))
            .row("Copied to the screen", Bytes(now.bytes_copied).to_string());
        Page::new("GPU")
            .subtitle(display_adapter(display))
            .content(column(
                space::XL,
                vec![
                    plotted(
                        "Busy",
                        String::from("100%"),
                        self.graph("GPU busy")
                            .line(display.busy.values(), GPU)
                            .max(WHOLE),
                    ),
                    plotted(
                        "Frames per second",
                        number(frame_ceiling),
                        self.graph("Frames per second")
                            .line(display.frames.values(), GPU)
                            .max(frame_ceiling),
                    ),
                    self.figures(figures),
                    Section::new("About", about).into(),
                ],
            ))
            .into()
    }
}

fn figure_node(figure: &Figure) -> Node<SysmonMsg> {
    let mut label: Vec<Node<SysmonMsg>> = Vec::new();
    if let Some(key) = figure.key {
        label.push(key.into());
    }
    label.push(caption(figure.label).into());
    column(
        space::XS,
        vec![
            row(space::XS, label),
            Text::new(figure.value.clone()).role(TextRole::Title).into(),
        ],
    )
}

fn share(part: u64, whole: u64) -> u64 {
    (u128::from(part) * u128::from(WHOLE) / u128::from(whole.max(1))) as u64
}
