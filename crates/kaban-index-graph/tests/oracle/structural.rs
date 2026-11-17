//! Plain structural oracles: breadth-first search over the oracle's edge list, written
//! independently of the crate (sets and maps, no merge, no budget).

use std::collections::{BTreeMap, BTreeSet, VecDeque};

use kaban_index_graph::{Direction, LocalId, RelId, RelSelection};

use super::{Oracle, selected};

/// The selected edges of the merged state whose endpoints are both present.
#[derive(Debug, Clone)]
pub(crate) struct Graph {
    pub(crate) nodes: BTreeSet<u32>,
    pub(crate) edges: BTreeSet<(u32, u16, u32)>,
    out: BTreeMap<u32, BTreeSet<u32>>,
    inc: BTreeMap<u32, BTreeSet<u32>>,
}

/// What the oracle expects from a bounded traversal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Expected {
    /// Reached nodes in (depth, id) order, sources excluded, cut at `rows`.
    pub(crate) nodes: Vec<(LocalId, u8)>,
    /// Without a cut: exactly the edges met. With a cut: every edge the result may report.
    pub(crate) edges: BTreeSet<(LocalId, RelId, LocalId)>,
    pub(crate) truncated: bool,
    /// Every node reached within the bound with its depth, sources at 0, before the cut.
    pub(crate) depth: BTreeMap<u32, u8>,
}

impl Oracle {
    /// The present subgraph over `rels`.
    pub(crate) fn graph(&self, rels: &RelSelection) -> Graph {
        let mut g = Graph {
            nodes: self.nodes.clone(),
            edges: BTreeSet::new(),
            out: BTreeMap::new(),
            inc: BTreeMap::new(),
        };
        for &(s, r, d) in self.edges.keys() {
            if selected(rels, r) && self.nodes.contains(&s) && self.nodes.contains(&d) {
                g.edges.insert((s, r, d));
                g.out.entry(s).or_default().insert(d);
                g.inc.entry(d).or_default().insert(s);
            }
        }
        g
    }
}

impl Graph {
    /// Distinct neighbours of `n` in `dir`.
    pub(crate) fn adj(&self, n: u32, dir: Direction) -> BTreeSet<u32> {
        let mut set = BTreeSet::new();
        if matches!(dir, Direction::Out | Direction::Both) {
            set.extend(self.out.get(&n).into_iter().flatten().copied());
        }
        if matches!(dir, Direction::In | Direction::Both) {
            set.extend(self.inc.get(&n).into_iter().flatten().copied());
        }
        set
    }

    /// The number of edges of `n` in `dir` (a self-loop counts once per direction).
    pub(crate) fn degree(&self, n: u32, dir: Direction) -> usize {
        self.edges
            .iter()
            .map(|&(s, _, d)| {
                let out = usize::from(s == n && matches!(dir, Direction::Out | Direction::Both));
                let inc = usize::from(d == n && matches!(dir, Direction::In | Direction::Both));
                out + inc
            })
            .sum()
    }

    /// Level-by-level breadth-first search from the present `src` up to `depth` hops, cut at the
    /// first `rows` nodes in (depth, id) order.
    pub(crate) fn expand(&self, src: &[LocalId], dir: Direction, depth: u8, rows: u32) -> Expected {
        let mut dist: BTreeMap<u32, u8> = BTreeMap::new();
        let mut frontier: BTreeSet<u32> = src
            .iter()
            .map(|n| n.0)
            .filter(|n| self.nodes.contains(n))
            .collect();
        for &n in &frontier {
            dist.insert(n, 0);
        }
        let mut order = Vec::new();
        for d in 1..=depth {
            let mut next = BTreeSet::new();
            for &n in &frontier {
                for m in self.adj(n, dir) {
                    if !dist.contains_key(&m) {
                        next.insert(m);
                    }
                }
            }
            for &m in &next {
                dist.insert(m, d);
                order.push((LocalId(m), d));
            }
            frontier = next;
        }
        let rows = usize::try_from(rows).unwrap();
        let truncated = order.len() > rows;
        order.truncate(rows);
        let kept: BTreeSet<u32> = dist
            .iter()
            .filter(|&(_, &d)| d == 0)
            .map(|(&n, _)| n)
            .chain(order.iter().map(|&(n, _)| n.0))
            .collect();
        let expanded = |n: u32| dist.get(&n).is_some_and(|&d| d < depth);
        let edges = self
            .edges
            .iter()
            .filter(|&&(s, _, d)| match dir {
                Direction::Out => expanded(s),
                Direction::In => expanded(d),
                Direction::Both => expanded(s) || expanded(d),
            })
            .filter(|&&(s, _, d)| !truncated || (kept.contains(&s) && kept.contains(&d)))
            .map(|&(s, r, d)| (LocalId(s), RelId(r), LocalId(d)))
            .collect();
        Expected {
            nodes: order,
            edges,
            truncated,
            depth: dist,
        }
    }

    /// Unbounded hop distances from `from` along `dir`.
    fn distances(&self, from: u32, dir: Direction) -> BTreeMap<u32, usize> {
        let mut dist = BTreeMap::from([(from, 0)]);
        let mut queue = VecDeque::from([from]);
        while let Some(n) = queue.pop_front() {
            let d = dist[&n];
            for m in self.adj(n, dir) {
                dist.entry(m).or_insert_with(|| {
                    queue.push_back(m);
                    d + 1
                });
            }
        }
        dist
    }

    /// The cap that cut the outgoing search from `src` within the caps, with the shallowest
    /// depth a node it did not keep can have: `rows` when more nodes lie within `depth` than
    /// `rows` (the floor is the depth of the first dropped node), `depth` when nodes lie past
    /// the depth bound (floor `depth + 1`); `None` when the search saw everything `src` reaches.
    fn cut(&self, src: u32, depth: u8, rows: u32) -> Option<(&'static str, u16)> {
        let all = self.expand(&[LocalId(src)], Direction::Out, depth, u32::MAX);
        if let Some(&(_, d)) = all.nodes.get(usize::try_from(rows).unwrap()) {
            return Some(("rows", u16::from(d)));
        }
        let far = self
            .distances(src, Direction::Out)
            .values()
            .any(|&d| d > usize::from(depth));
        far.then_some(("depth", u16::from(depth) + 1))
    }

    /// Whether `dst` is reachable from `src` (zero hops when equal and present), or the cap
    /// that kept the search from settling it: `true` when `dst` is among the nodes reached
    /// within the caps, `false` only when the search saw everything `src` reaches.
    pub(crate) fn reachable(
        &self,
        src: u32,
        dst: u32,
        depth: u8,
        rows: u32,
    ) -> Result<bool, &'static str> {
        if src == dst {
            return Ok(self.nodes.contains(&src));
        }
        let reached = self
            .expand(&[LocalId(src)], Direction::Out, depth, rows)
            .nodes
            .iter()
            .any(|&(n, _)| n.0 == dst);
        if reached {
            return Ok(true);
        }
        if let Some((cap, _)) = self.cut(src, depth, rows) {
            return Err(cap);
        }
        let far = self.distances(src, Direction::Out);
        assert!(
            !far.contains_key(&dst),
            "an uncut search saw every reachable node"
        );
        Ok(false)
    }

    /// The lexicographically smallest shortest path from `src` to `dst` (at every step the
    /// lowest-id successor still on a shortest path), `None` when `dst` is unreachable, or the
    /// cap that kept the search from settling it, as [`Self::reachable`] decides.
    pub(crate) fn shortest_path(
        &self,
        src: u32,
        dst: u32,
        depth: u8,
        rows: u32,
    ) -> Result<Option<Vec<LocalId>>, &'static str> {
        if !self.reachable(src, dst, depth, rows)? {
            return Ok(None);
        }
        let from = self.distances(src, Direction::Out);
        let to = self.distances(dst, Direction::In);
        let len = from[&dst];
        let mut path = vec![LocalId(src)];
        let mut cur = src;
        for step in 1..=len {
            let Some(next) = self
                .adj(cur, Direction::Out)
                .into_iter()
                .find(|m| from.get(m) == Some(&step) && to.get(m) == Some(&(len - step)))
            else {
                return Ok(None);
            };
            cur = next;
            path.push(LocalId(cur));
        }
        Ok(Some(path))
    }

    /// Distinct neighbours shared by `a` and `b`, ignoring direction.
    pub(crate) fn common(&self, a: u32, b: u32, dir: Direction) -> Vec<LocalId> {
        let na = self.adj(a, dir);
        let nb = self.adj(b, dir);
        na.intersection(&nb).map(|&n| LocalId(n)).collect()
    }

    /// Nodes reached from both `a` and `b` (each at depth 0 from itself) with the smallest
    /// summed depth, ascending, or the refused cap: the cut cap when a node the bounded searches
    /// missed could have a summed depth at most the found minimum (or nothing was found and a
    /// search was cut), `rows` when more than `rows` nodes share the minimum.
    ///
    /// A missed node has a summed depth of at least the smallest cut floor, so a minimum below
    /// it is the exact one; that is checked here against the unbounded distances.
    pub(crate) fn meet(
        &self,
        a: u32,
        b: u32,
        depth: u8,
        rows: u32,
    ) -> Result<Vec<LocalId>, &'static str> {
        let minima = |da: &BTreeMap<u32, usize>,
                      db: &BTreeMap<u32, usize>|
         -> (Option<usize>, Vec<LocalId>) {
            let sums: BTreeMap<u32, usize> = da
                .iter()
                .filter_map(|(n, &x)| db.get(n).map(|&y| (*n, x + y)))
                .collect();
            let best = sums.values().min().copied();
            let out = sums
                .into_iter()
                .filter(|&(_, s)| Some(s) == best)
                .map(|(n, _)| LocalId(n))
                .collect();
            (best, out)
        };
        let widen =
            |m: BTreeMap<u32, u8>| m.into_iter().map(|(n, d)| (n, usize::from(d))).collect();
        let (best, found) = minima(
            &widen(self.reached(a, depth, rows)),
            &widen(self.reached(b, depth, rows)),
        );
        let low = [self.cut(a, depth, rows), self.cut(b, depth, rows)]
            .into_iter()
            .flatten()
            .min_by_key(|&(_, floor)| floor);
        if let Some((cap, floor)) = low
            && best.is_none_or(|s| s >= usize::from(floor))
        {
            return Err(cap);
        }
        let present = |n: u32| -> BTreeMap<u32, usize> {
            if self.nodes.contains(&n) {
                self.distances(n, Direction::Out)
            } else {
                BTreeMap::new()
            }
        };
        let (_, exact) = minima(&present(a), &present(b));
        assert_eq!(found, exact, "a settled meet is the unbounded one");
        if found.len() > usize::try_from(rows).unwrap() {
            return Err("rows");
        }
        Ok(found)
    }

    /// Present `n` at 0 plus the reached nodes, within the caps.
    fn reached(&self, n: u32, depth: u8, rows: u32) -> BTreeMap<u32, u8> {
        let e = self.expand(&[LocalId(n)], Direction::Out, depth, rows);
        let mut map: BTreeMap<u32, u8> = e.nodes.iter().map(|&(m, d)| (m.0, d)).collect();
        if self.nodes.contains(&n) {
            map.insert(n, 0);
        }
        map
    }

    /// The set a topological order covers: the present `nodes` and what they reach within the
    /// caps; `None` when the rows cap cuts the search or the complete set, sources included, has
    /// more than `rows` nodes (`rows` bounds the result).
    pub(crate) fn topo_set(
        &self,
        nodes: &[LocalId],
        depth: u8,
        rows: u32,
    ) -> Option<BTreeSet<u32>> {
        let e = self.expand(nodes, Direction::Out, depth, rows);
        let set: BTreeSet<u32> = e.depth.keys().copied().collect();
        if e.truncated || set.len() > rows as usize {
            return None;
        }
        Some(set)
    }

    /// Whether `order` lists exactly `set`, every edge inside `set` going forward.
    pub(crate) fn is_topo_order(&self, set: &BTreeSet<u32>, order: &[LocalId]) -> bool {
        let pos: BTreeMap<u32, usize> = order.iter().enumerate().map(|(i, n)| (n.0, i)).collect();
        pos.len() == order.len()
            && pos.keys().copied().collect::<BTreeSet<_>>() == *set
            && self
                .edges
                .iter()
                .filter(|(s, _, d)| set.contains(s) && set.contains(d))
                .all(|(s, _, d)| pos[s] < pos[d])
    }

    /// Whether `n` lies on a cycle of edges inside `set`.
    pub(crate) fn on_cycle(&self, set: &BTreeSet<u32>, n: u32) -> bool {
        let mut seen = BTreeSet::new();
        let mut stack: Vec<u32> = self.adj(n, Direction::Out).into_iter().collect();
        while let Some(m) = stack.pop() {
            if !set.contains(&m) || !seen.insert(m) {
                continue;
            }
            if m == n {
                return true;
            }
            stack.extend(self.adj(m, Direction::Out));
        }
        false
    }

    /// Whether any node of `set` lies on a cycle inside it.
    pub(crate) fn has_cycle(&self, set: &BTreeSet<u32>) -> bool {
        set.iter().any(|&n| self.on_cycle(set, n))
    }

    /// (node, lowest member of its weak component) for every present node, ascending, cut at
    /// `rows`, and whether the cut left rows out.
    pub(crate) fn wcc(&self, rows: u32) -> (Vec<(LocalId, LocalId)>, bool) {
        let mut comp: BTreeMap<u32, u32> = BTreeMap::new();
        for &start in &self.nodes {
            if comp.contains_key(&start) {
                continue;
            }
            // Ascending iteration: `start` is the lowest member of its component.
            let mut queue = VecDeque::from([start]);
            comp.insert(start, start);
            while let Some(n) = queue.pop_front() {
                for m in self.adj(n, Direction::Both) {
                    if let std::collections::btree_map::Entry::Vacant(e) = comp.entry(m) {
                        e.insert(start);
                        queue.push_back(m);
                    }
                }
            }
        }
        let rows = usize::try_from(rows).unwrap();
        let truncated = comp.len() > rows;
        let kept = comp
            .into_iter()
            .take(rows)
            .map(|(n, c)| (LocalId(n), LocalId(c)))
            .collect();
        (kept, truncated)
    }
}
