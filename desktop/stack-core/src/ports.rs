//! Slot port arithmetic.
//!
//! A component's host port is `portBase + slot * portStride + index`. Each
//! component owns a run of consecutive indexes (its `ports:` count, default
//! one for anything that publishes locally, none for terraform). Indexes are
//! handed out in definition order, skipping any a component pinned with
//! `portIndex`, so adding a component at the end never renumbers the others.
//!
//! Ports are allocated **host-wide**: every engine of an instance publishes
//! onto the same host, so the layout is one table for the whole instance,
//! not one per engine, and a collision between a docker component and a
//! podman component is caught like any other.

use indexmap::IndexMap;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

use crate::definition::StackDef;

pub const MAX_PORT: u32 = u16::MAX as u32;

/// A component's run of indexes within a slot's stride.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IndexRun {
    pub index: u16,
    pub count: u16,
}

impl IndexRun {
    fn end(self) -> u32 {
        self.index as u32 + self.count as u32
    }

    fn overlaps(self, other: IndexRun) -> bool {
        self.count > 0
            && other.count > 0
            && (self.index as u32) < other.end()
            && (other.index as u32) < self.end()
    }
}

/// The port table of one stack: enough to compute any slot's ports.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Layout {
    pub base: u16,
    pub stride: u16,
    pub slots: u32,
    pub components: IndexMap<String, IndexRun>,
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum PortError {
    #[error("portStride must be at least 1")]
    ZeroStride,
    #[error("the components need {needed} port index(es) but portStride is {stride}; raise the stride or publish fewer ports")]
    StrideTooSmall { needed: u32, stride: u16 },
    #[error("components `{component}` and `{other}` both claim port index {index}")]
    IndexOverlap {
        component: String,
        other: String,
        index: u16,
    },
    #[error("slot {slot} would use ports up to {last_port}, past {MAX_PORT}; lower portBase, portStride or the slot count")]
    RangeOverflow { slot: u32, last_port: u64 },
    #[error("slot {slot} is outside the stack's {count} slot(s)")]
    SlotOutOfRange { slot: u32, count: u32 },
}

/// Assign indexes and check the whole table fits every slot.
pub fn layout(stack: &StackDef) -> Result<Layout, PortError> {
    let stride = stack.slots.port_stride;
    if stride == 0 {
        return Err(PortError::ZeroStride);
    }
    let mut components: IndexMap<String, IndexRun> = IndexMap::new();
    // Pinned runs first, so the free ones route around them.
    for (name, def) in &stack.components {
        if let Some(index) = def.port_index {
            let run = IndexRun {
                index,
                count: def.port_count(),
            };
            check_overlap(&components, name, run)?;
            components.insert(name.clone(), run);
        }
    }
    let mut next: u16 = 0;
    for (name, def) in &stack.components {
        if def.port_index.is_some() {
            continue;
        }
        let count = def.port_count();
        if count == 0 {
            components.insert(name.clone(), IndexRun { index: 0, count: 0 });
            continue;
        }
        let mut run = IndexRun { index: next, count };
        while components.values().any(|r| r.overlaps(run)) {
            run.index += 1;
        }
        next = run.end().min(MAX_PORT) as u16;
        components.insert(name.clone(), run);
    }
    // Keep definition order in the table, whichever pass inserted first.
    let mut ordered = IndexMap::new();
    for name in stack.components.keys() {
        ordered.insert(name.clone(), components[name]);
    }
    let needed = ordered.values().map(|r| r.end()).max().unwrap_or(0);
    if needed > stride as u32 {
        return Err(PortError::StrideTooSmall { needed, stride });
    }
    let out = Layout {
        base: stack.slots.port_base,
        stride,
        slots: stack.slots.count,
        components: ordered,
    };
    if stack.slots.count > 0 {
        let last_slot = stack.slots.count - 1;
        let last_port =
            out.base as u64 + last_slot as u64 * stride as u64 + needed.saturating_sub(1) as u64;
        if needed > 0 && last_port > MAX_PORT as u64 {
            return Err(PortError::RangeOverflow {
                slot: last_slot,
                last_port,
            });
        }
    }
    Ok(out)
}

fn check_overlap(
    taken: &IndexMap<String, IndexRun>,
    name: &str,
    run: IndexRun,
) -> Result<(), PortError> {
    for (other, r) in taken {
        if r.overlaps(run) {
            return Err(PortError::IndexOverlap {
                component: name.to_string(),
                other: other.clone(),
                index: run.index.max(r.index),
            });
        }
    }
    Ok(())
}

impl Layout {
    /// One port: `base + slot * stride + index`.
    pub fn port(&self, slot: u32, index: u16) -> Result<u16, PortError> {
        if slot >= self.slots {
            return Err(PortError::SlotOutOfRange {
                slot,
                count: self.slots,
            });
        }
        let p = self.base as u64 + slot as u64 * self.stride as u64 + index as u64;
        u16::try_from(p).map_err(|_| PortError::RangeOverflow { slot, last_port: p })
    }

    /// Every component's ports in one slot, definition order. Components
    /// that publish nothing map to an empty list.
    pub fn instance_ports(&self, slot: u32) -> Result<IndexMap<String, Vec<u16>>, PortError> {
        let mut out = IndexMap::new();
        for (name, run) in &self.components {
            let mut ports = Vec::with_capacity(run.count as usize);
            for i in 0..run.count {
                ports.push(self.port(slot, run.index + i)?);
            }
            out.insert(name.clone(), ports);
        }
        Ok(out)
    }

    /// Every port any slot of this stack may publish.
    pub fn all_ports(&self) -> Result<BTreeSet<u16>, PortError> {
        let mut out = BTreeSet::new();
        for slot in 0..self.slots {
            for ports in self.instance_ports(slot)?.values() {
                out.extend(ports.iter().copied());
            }
        }
        Ok(out)
    }
}

/// Who holds a colliding port.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case", tag = "kind")]
pub enum Holder {
    /// Something outside this instance already listens on it.
    InUse,
    /// Another component of the same instance.
    Component { name: String },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Collision {
    pub component: String,
    pub port: u16,
    pub holder: Holder,
}

/// Collisions of an instance's ports with each other and with `in_use`
/// (ports already bound on the host, whichever engine or process owns
/// them). Pairwise collisions are reported once, on the later component.
pub fn collisions(ports: &IndexMap<String, Vec<u16>>, in_use: &BTreeSet<u16>) -> Vec<Collision> {
    let mut out = Vec::new();
    let mut seen: IndexMap<u16, &str> = IndexMap::new();
    for (component, list) in ports {
        for &port in list {
            if in_use.contains(&port) {
                out.push(Collision {
                    component: component.clone(),
                    port,
                    holder: Holder::InUse,
                });
            }
            if let Some(other) = seen.get(&port) {
                out.push(Collision {
                    component: component.clone(),
                    port,
                    holder: Holder::Component {
                        name: other.to_string(),
                    },
                });
            } else {
                seen.insert(port, component);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::definition::{ComponentDef, DriverKind, GroupDef, SlotsDef};

    fn stack(
        stride: u16,
        count: u32,
        comps: &[(&str, &str, Option<u16>, Option<u16>)],
    ) -> StackDef {
        // (name, group, ports, portIndex)
        let mut s = StackDef {
            slots: SlotsDef {
                count,
                port_base: 14000,
                port_stride: stride,
                ..SlotsDef::default()
            },
            ..StackDef::default()
        };
        s.groups.insert(
            "core".into(),
            GroupDef {
                engine: Some("docker".into()),
                label: None,
            },
        );
        s.groups.insert(
            "apis".into(),
            GroupDef {
                engine: Some("podman".into()),
                label: None,
            },
        );
        for (name, group, ports, index) in comps {
            let mut c = ComponentDef::new(DriverKind::Compose);
            c.group = Some(group.to_string());
            c.ports = *ports;
            c.port_index = *index;
            s.components.insert(name.to_string(), c);
        }
        s
    }

    #[test]
    fn indexes_follow_definition_order() {
        let s = stack(
            100,
            4,
            &[
                ("db", "apis", None, None),
                ("api", "apis", None, None),
                ("web", "core", Some(2), None),
            ],
        );
        let l = layout(&s).unwrap();
        assert_eq!(l.components["db"], IndexRun { index: 0, count: 1 });
        assert_eq!(l.components["api"], IndexRun { index: 1, count: 1 });
        assert_eq!(l.components["web"], IndexRun { index: 2, count: 2 });
        let p = l.instance_ports(2).unwrap();
        assert_eq!(p["db"], vec![14200]);
        assert_eq!(p["api"], vec![14201]);
        assert_eq!(p["web"], vec![14202, 14203]);
    }

    #[test]
    fn slot_zero_is_the_base() {
        let s = stack(100, 1, &[("api", "apis", None, None)]);
        assert_eq!(
            layout(&s).unwrap().instance_ports(0).unwrap()["api"],
            vec![14000]
        );
    }

    #[test]
    fn pinned_indexes_are_routed_around() {
        let s = stack(
            10,
            1,
            &[
                ("a", "core", None, None),
                ("b", "core", None, Some(0)),
                ("c", "core", Some(2), Some(2)),
                ("d", "core", None, None),
            ],
        );
        let l = layout(&s).unwrap();
        assert_eq!(l.components["b"].index, 0);
        assert_eq!(l.components["a"].index, 1);
        assert_eq!(l.components["c"], IndexRun { index: 2, count: 2 });
        assert_eq!(l.components["d"].index, 4);
        assert_eq!(
            l.components.keys().collect::<Vec<_>>(),
            ["a", "b", "c", "d"]
        );
    }

    #[test]
    fn zero_port_components_take_no_index() {
        let s = stack(
            10,
            1,
            &[
                ("infra", "core", Some(0), None),
                ("api", "core", None, None),
            ],
        );
        let l = layout(&s).unwrap();
        assert_eq!(l.components["infra"].count, 0);
        assert_eq!(l.components["api"].index, 0);
        assert!(l.instance_ports(0).unwrap()["infra"].is_empty());
    }

    #[test]
    fn stride_edges() {
        // three single-port components: stride 3 fits exactly, 2 does not
        let fits = stack(
            3,
            2,
            &[
                ("a", "core", None, None),
                ("b", "core", None, None),
                ("c", "apis", None, None),
            ],
        );
        assert!(layout(&fits).is_ok());
        let p = layout(&fits).unwrap().instance_ports(1).unwrap();
        assert_eq!(p["a"], vec![14003]);
        let tight = stack(
            2,
            2,
            &[
                ("a", "core", None, None),
                ("b", "core", None, None),
                ("c", "apis", None, None),
            ],
        );
        assert_eq!(
            layout(&tight).unwrap_err(),
            PortError::StrideTooSmall {
                needed: 3,
                stride: 2
            }
        );
        // a pinned index at the stride edge also overflows it
        let pinned = stack(3, 1, &[("a", "core", None, Some(3))]);
        assert_eq!(
            layout(&pinned).unwrap_err(),
            PortError::StrideTooSmall {
                needed: 4,
                stride: 3
            }
        );
        let zero = stack(0, 1, &[("a", "core", None, None)]);
        assert_eq!(layout(&zero).unwrap_err(), PortError::ZeroStride);
    }

    #[test]
    fn explicit_index_overlap_across_engines_is_an_error() {
        let s = stack(
            10,
            1,
            &[
                ("api", "apis", None, Some(0)),
                ("web", "core", None, Some(0)),
            ],
        );
        assert_eq!(
            layout(&s).unwrap_err(),
            PortError::IndexOverlap {
                component: "web".into(),
                other: "api".into(),
                index: 0
            }
        );
        let s = stack(
            10,
            1,
            &[
                ("api", "apis", Some(3), Some(0)),
                ("web", "core", None, Some(2)),
            ],
        );
        assert!(matches!(
            layout(&s).unwrap_err(),
            PortError::IndexOverlap { index: 2, .. }
        ));
    }

    #[test]
    fn port_range_overflow() {
        let mut s = stack(100, 4, &[("api", "core", None, None)]);
        s.slots.port_base = 65300;
        // slot 3 → 65300 + 300 = 65600
        assert_eq!(
            layout(&s).unwrap_err(),
            PortError::RangeOverflow {
                slot: 3,
                last_port: 65600
            }
        );
        s.slots.count = 3; // slot 2 → 65500: fits
        let l = layout(&s).unwrap();
        assert_eq!(l.instance_ports(2).unwrap()["api"], vec![65500]);
        assert_eq!(
            l.port(3, 0).unwrap_err(),
            PortError::SlotOutOfRange { slot: 3, count: 3 }
        );
        // the top port itself is fine
        let mut top = stack(1, 1, &[("api", "core", None, None)]);
        top.slots.port_base = 65535;
        assert_eq!(
            layout(&top).unwrap().instance_ports(0).unwrap()["api"],
            vec![65535]
        );
    }

    #[test]
    fn all_ports_covers_every_slot() {
        let s = stack(
            10,
            3,
            &[("a", "core", None, None), ("b", "apis", Some(2), None)],
        );
        let all = layout(&s).unwrap().all_ports().unwrap();
        assert_eq!(
            all.into_iter().collect::<Vec<_>>(),
            vec![14000, 14001, 14002, 14010, 14011, 14012, 14020, 14021, 14022]
        );
    }

    #[test]
    fn collisions_with_in_use_and_between_components() {
        let mut ports = IndexMap::new();
        ports.insert("api".to_string(), vec![14000, 14001]);
        ports.insert("web".to_string(), vec![14001]); // overlaps api on purpose
        ports.insert("db".to_string(), vec![14002]);
        let in_use: BTreeSet<u16> = [14000, 14002, 9999].into_iter().collect();
        let found = collisions(&ports, &in_use);
        assert_eq!(
            found,
            vec![
                Collision {
                    component: "api".into(),
                    port: 14000,
                    holder: Holder::InUse
                },
                Collision {
                    component: "web".into(),
                    port: 14001,
                    holder: Holder::Component { name: "api".into() }
                },
                Collision {
                    component: "db".into(),
                    port: 14002,
                    holder: Holder::InUse
                },
            ]
        );
        assert!(collisions(&ports, &BTreeSet::new()).len() == 1);
    }

    #[test]
    fn no_collisions_when_everything_is_free() {
        let s = stack(
            10,
            2,
            &[("a", "core", None, None), ("b", "apis", None, None)],
        );
        let l = layout(&s).unwrap();
        let p = l.instance_ports(1).unwrap();
        // ports of slot 0 being in use never collides with slot 1
        let in_use = l
            .instance_ports(0)
            .unwrap()
            .values()
            .flatten()
            .copied()
            .collect();
        assert!(collisions(&p, &in_use).is_empty());
    }

    #[test]
    fn holder_serializes_tagged() {
        let c = Collision {
            component: "api".into(),
            port: 1,
            holder: Holder::Component { name: "web".into() },
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(
            json.contains("\"holder\":{\"kind\":\"component\",\"name\":\"web\"}"),
            "{json}"
        );
        let i = Collision {
            component: "api".into(),
            port: 1,
            holder: Holder::InUse,
        };
        assert!(serde_json::to_string(&i)
            .unwrap()
            .contains("\"holder\":{\"kind\":\"in-use\"}"));
    }
}
