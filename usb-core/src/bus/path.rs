//! A root port and the hub ports below it: the name (`3.2` is port 2 of the
//! hub on root port 3) and the route string (xHCI 1.2 §8.9).

use core::fmt;

use crate::hub::{MAX_DEPTH, MAX_HUB_PORTS};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct Path {
    root: u8,
    hops: [u8; MAX_DEPTH as usize],
    depth: u8,
}

impl Path {
    pub fn root(port: u8) -> Self {
        Self {
            root: port,
            ..Self::default()
        }
    }

    /// Port `port` of the hub at this path; `None` below the fifth tier.
    pub fn child(&self, port: u8) -> Option<Self> {
        if self.depth >= MAX_DEPTH {
            return None;
        }
        let mut child = *self;
        child.hops[usize::from(self.depth)] = port;
        child.depth += 1;
        Some(child)
    }

    pub fn root_port(&self) -> u8 {
        self.root
    }

    /// Hubs above the device; also what `SET_HUB_DEPTH` gives a hub here.
    pub fn depth(&self) -> u8 {
        self.depth
    }

    /// Four bits per tier, the first hub's port lowest, saturating at 15.
    pub fn route_string(&self) -> u32 {
        self.hops[..usize::from(self.depth)]
            .iter()
            .enumerate()
            .fold(0, |route, (tier, &port)| {
                route | u32::from(port.min(MAX_HUB_PORTS)) << (4 * tier)
            })
    }
}

impl fmt::Display for Path {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.root)?;
        for hop in &self.hops[..usize::from(self.depth)] {
            write!(f, ".{}", hop)?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::string::ToString;

    #[test]
    fn routes_name_each_tier_by_its_port() {
        let root = Path::root(3);
        assert_eq!((root.route_string(), root.depth()), (0, 0));
        assert_eq!(root.to_string(), "3");
        let hub = root.child(2).unwrap();
        let leaf = hub.child(7).unwrap();
        assert_eq!(leaf.route_string(), 0x72);
        assert_eq!(leaf.depth(), 2);
        assert_eq!(leaf.root_port(), 3);
        assert_eq!(leaf.to_string(), "3.2.7");
        let wide = root.child(200).unwrap();
        assert_eq!(wide.route_string(), 0xf);
        let deepest = (0..5).fold(root, |p, _| p.child(1).unwrap());
        assert_eq!(deepest.route_string(), 0x11111);
        assert_eq!(deepest.child(1), None);
    }
}
