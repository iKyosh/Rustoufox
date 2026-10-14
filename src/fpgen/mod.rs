//! A port of [fpgen](https://github.com/scrapfly/fingerprint-generator), the
//! fingerprint generator Camoufox draws its identities from.
//!
//! fpgen is a Bayesian network over browser properties, recorded from real
//! traffic. Each node's values are stored as indices into `values.dat` (JSON
//! fragments), and each node's conditional probability table is keyed by its
//! parents' value indices. Sampling walks the nodes in order; every node is
//! drawn from the distribution a beam search computes given what is already
//! known, so conditions on any node (an OS, a screen bound) constrain the whole
//! sample consistently.

pub mod model;

use std::collections::{HashMap, HashSet};
use std::fs::File;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::{Arc, Mutex};

use serde_json::{Map, Value};

use crate::error::{Error, Result};

/// Width of the beam search: cuts off values that are too unlikely to matter.
const BEAM_WIDTH: usize = 1000;

/// A condition on one node (or one field inside a node's value).
pub enum Condition {
    /// The value must equal this one (compared case-insensitively, as fpgen does).
    Equals(Value),
    /// The value must equal one of these.
    OneOf(Vec<Value>),
    /// The value must satisfy this predicate.
    Predicate(Box<dyn Fn(&Value) -> bool + Send + Sync>),
}

impl std::fmt::Debug for Condition {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Condition::Equals(v) => write!(f, "Equals({v})"),
            Condition::OneOf(v) => write!(f, "OneOf({v:?})"),
            Condition::Predicate(_) => write!(f, "Predicate(..)"),
        }
    }
}

/// A node's values as (casefolded JSON, id).
type Possibilities = Arc<Vec<(String, u32)>>;

/// Allowed value ids per node.
pub type Evidence = HashMap<usize, HashSet<u32>>;

#[derive(Debug, Clone)]
enum Cpt {
    Leaf(Vec<(u32, f64)>),
    Branch(HashMap<u32, Cpt>),
}

#[derive(Debug)]
struct Node {
    name: String,
    parents: Vec<usize>,
    possible: Vec<u32>,
    cpt: Cpt,
}

/// One value of a traced distribution.
#[derive(Debug, Clone)]
pub struct TraceResult {
    /// The value's id in the network (stable for the life of the process).
    pub id: u32,
    pub value: Value,
    pub probability: f64,
}

pub struct Network {
    nodes: Vec<Node>,
    by_name: HashMap<String, usize>,
    ancestors: Vec<Vec<bool>>,
    /// Interned value index strings ("S^", "!", ...).
    strings: Vec<String>,
    /// (offset, length) in values.dat, by base85 index.
    pairs: Vec<(u64, usize)>,
    dat: Mutex<File>,
    cache: Mutex<HashMap<u32, Arc<str>>>,
    possibilities: Mutex<HashMap<usize, Possibilities>>,
}

impl std::fmt::Debug for Network {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Network").field("nodes", &self.nodes.len()).finish()
    }
}

/// Python's `base64.b85decode(s)` read as a big-endian integer.
fn base85_to_int(s: &str) -> u64 {
    const ALPHABET: &[u8] = b"0123456789ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz!#$%&()*+-;<=>?@^_`{|}~";
    let mut digits: Vec<u32> = s.bytes().map(|b| ALPHABET.iter().position(|&a| a == b).unwrap_or(0) as u32).collect();
    let padding = (5 - digits.len() % 5) % 5;
    digits.extend(std::iter::repeat_n(84, padding));
    let mut bytes = Vec::new();
    for chunk in digits.chunks(5) {
        let mut acc: u64 = 0;
        for d in chunk {
            acc = acc * 85 + *d as u64;
        }
        bytes.extend_from_slice(&(acc as u32).to_be_bytes());
    }
    bytes.truncate(bytes.len() - padding);
    bytes.iter().fold(0u64, |acc, b| (acc << 8) | *b as u64)
}

fn casefold(s: &str) -> String {
    s.to_lowercase()
}

impl Network {
    /// Load the network from a model directory.
    pub fn load(dir: &Path) -> Result<Network> {
        let network: Value = serde_json::from_slice(&model::read_zst(&dir.join("fingerprint-network.json.zst"))?)?;
        let values: Map<String, Value> = serde_json::from_slice(&model::read_zst(&dir.join("values.json.zst"))?)?;
        let pairs = values
            .iter()
            .map(|(offset, len)| {
                let off = u64::from_str_radix(offset, 16)
                    .map_err(|_| Error::FpgenModel(format!("bad offset {offset} in values.json")))?;
                Ok((off, len.as_u64().unwrap_or(0) as usize))
            })
            .collect::<Result<Vec<_>>>()?;

        let mut strings: Vec<String> = Vec::new();
        let mut ids: HashMap<String, u32> = HashMap::new();
        let mut intern = |s: &str| -> u32 {
            if let Some(id) = ids.get(s) {
                return *id;
            }
            let id = strings.len() as u32;
            strings.push(s.to_string());
            ids.insert(s.to_string(), id);
            id
        };

        let raw_nodes = network["nodes"]
            .as_array()
            .ok_or_else(|| Error::FpgenModel("fingerprint-network.json has no nodes".into()))?;
        let mut by_name = HashMap::new();
        for (i, raw) in raw_nodes.iter().enumerate() {
            by_name.insert(casefold(raw["name"].as_str().unwrap_or_default()), i);
        }

        fn parse_cpt(v: &Value, depth: usize, intern: &mut dyn FnMut(&str) -> u32) -> Cpt {
            let obj = v.as_object();
            if depth == 0 {
                Cpt::Leaf(
                    obj.map(|o| o.iter().map(|(k, p)| (intern(k), p.as_f64().unwrap_or(0.0))).collect())
                        .unwrap_or_default(),
                )
            } else {
                Cpt::Branch(
                    obj.map(|o| o.iter().map(|(k, sub)| (intern(k), parse_cpt(sub, depth - 1, intern))).collect())
                        .unwrap_or_default(),
                )
            }
        }

        let mut nodes = Vec::with_capacity(raw_nodes.len());
        for raw in raw_nodes {
            let parents: Vec<usize> = raw["parentNames"]
                .as_array()
                .map(|a| a.iter().filter_map(|p| p.as_str().and_then(|p| by_name.get(&casefold(p)).copied())).collect())
                .unwrap_or_default();
            let possible: Vec<u32> = raw["possibleValues"]
                .as_array()
                .map(|a| a.iter().filter_map(|v| v.as_str()).map(&mut intern).collect())
                .unwrap_or_default();
            let cpt = parse_cpt(&raw["conditionalProbabilities"], parents.len(), &mut intern);
            nodes.push(Node { name: raw["name"].as_str().unwrap_or_default().to_string(), parents, possible, cpt });
        }

        // Ancestors: a node's parents always come earlier in the sampling order.
        let n = nodes.len();
        let mut ancestors = vec![vec![false; n]; n];
        for i in 0..n {
            for &p in &nodes[i].parents.clone() {
                ancestors[i][p] = true;
                let parent_anc = ancestors[p].clone();
                for (j, is) in parent_anc.into_iter().enumerate() {
                    if is {
                        ancestors[i][j] = true;
                    }
                }
            }
        }

        Ok(Network {
            nodes,
            by_name,
            ancestors,
            strings,
            pairs,
            dat: Mutex::new(File::open(dir.join("values.dat"))?),
            cache: Mutex::new(HashMap::new()),
            possibilities: Mutex::new(HashMap::new()),
        })
    }

    pub fn node_index(&self, name: &str) -> Option<usize> {
        self.by_name.get(&casefold(name)).copied()
    }

    pub fn node_name(&self, index: usize) -> &str {
        &self.nodes[index].name
    }

    /// The JSON text of a value id.
    pub fn lookup(&self, id: u32) -> Result<Arc<str>> {
        if let Some(v) = self.cache.lock().unwrap().get(&id) {
            return Ok(v.clone());
        }
        let index = base85_to_int(&self.strings[id as usize]) as usize;
        let (offset, len) =
            *self.pairs.get(index).ok_or_else(|| Error::FpgenModel(format!("value index {index} out of range")))?;
        let mut buf = vec![0u8; len];
        {
            let mut f = self.dat.lock().unwrap();
            f.seek(SeekFrom::Start(offset))?;
            f.read_exact(&mut buf)?;
        }
        let text: Arc<str> = String::from_utf8_lossy(&buf).into_owned().into();
        self.cache.lock().unwrap().insert(id, text.clone());
        Ok(text)
    }

    pub fn lookup_value(&self, id: u32) -> Result<Value> {
        Ok(serde_json::from_str(&self.lookup(id)?)?)
    }

    /// Every possible value of a node: (casefolded JSON, id).
    fn possibilities(&self, node: usize) -> Result<Possibilities> {
        if let Some(p) = self.possibilities.lock().unwrap().get(&node) {
            return Ok(p.clone());
        }
        let mut out = Vec::with_capacity(self.nodes[node].possible.len());
        for &id in &self.nodes[node].possible {
            out.push((casefold(&self.lookup(id)?), id));
        }
        let out = Arc::new(out);
        self.possibilities.lock().unwrap().insert(node, out.clone());
        Ok(out)
    }

    /// Every possible value of a node with its id, exactly as stored.
    pub fn possible_values(&self, node: &str) -> Result<Vec<(u32, Value)>> {
        let index = self.node_index(node).ok_or_else(|| Error::InvalidNode(format!("{node} is not a valid node")))?;
        self.nodes[index].possible.iter().map(|&id| Ok((id, self.lookup_value(id)?))).collect()
    }

    /// Resolve a (possibly nested) key into its node and the path inside the node's value.
    fn resolve_key(&self, key: &str) -> Result<(usize, Vec<String>)> {
        if let Some(i) = self.node_index(key) {
            return Ok((i, Vec::new()));
        }
        let mut nested = Vec::new();
        let mut key = key.to_string();
        loop {
            let Some((head, tail)) = key.rsplit_once('.') else {
                return Err(Error::InvalidNode(format!("{key} is not a valid node")));
            };
            nested.push(casefold(tail));
            key = head.to_string();
            if let Some(i) = self.node_index(&key) {
                nested.reverse();
                return Ok((i, nested));
            }
        }
    }

    /// Turn conditions into evidence, and check it is satisfiable.
    pub fn build_evidence(&self, conditions: &[(&str, Condition)], evidence: &mut Evidence) -> Result<()> {
        for (key, condition) in conditions {
            let (node, nested) = self.resolve_key(&casefold(key))?;
            let possibilities = self.possibilities(node)?;
            let mut allowed = HashSet::new();
            let wanted: Vec<Option<Value>> = match condition {
                Condition::Equals(v) => vec![Some(v.clone())],
                Condition::OneOf(vs) => vs.iter().cloned().map(Some).collect(),
                Condition::Predicate(_) => vec![None],
            };
            for want in wanted {
                let want_folded: Option<Value> = match &want {
                    Some(v) => Some(serde_json::from_str(&casefold(&serde_json::to_string(v)?))?),
                    None => None,
                };
                let mut found = false;
                for (folded, id) in possibilities.iter() {
                    let parsed: Value = serde_json::from_str(folded)?;
                    let target = if nested.is_empty() { Some(&parsed) } else { at_path(&parsed, &nested) };
                    let Some(target) = target else { continue };
                    let ok = match (&want_folded, condition) {
                        (Some(w), _) => target == w,
                        (None, Condition::Predicate(f)) => f(target),
                        _ => false,
                    };
                    if ok {
                        allowed.insert(*id);
                        found = true;
                    }
                }
                if !found {
                    let at = if nested.is_empty() { String::new() } else { format!(" at \"{}\"", nested.join(".")) };
                    let name = &self.nodes[node].name;
                    return Err(Error::InvalidConstraints(match want {
                        Some(v) => format!("{v} is not a possible value for \"{name}\"{at}"),
                        None => format!("The passed function yielded no possible values for \"{name}\"{at}"),
                    }));
                }
            }
            evidence.insert(node, allowed);
        }
        self.validate_evidence(evidence)
    }

    /// Check that each constrained node is possible given the other single-valued constraints.
    pub fn validate_evidence(&self, evidence: &Evidence) -> Result<()> {
        if evidence.len() <= 1 {
            return Ok(());
        }
        for (&node, allowed) in evidence {
            let fixed: Evidence =
                evidence.iter().filter(|(k, v)| **k != node && v.len() == 1).map(|(k, v)| (*k, v.clone())).collect();
            if fixed.is_empty() {
                continue;
            }
            let dist = self.trace_ids(node, &fixed);
            if !dist.is_empty() && allowed.iter().all(|v| dist.iter().all(|(id, p)| id != v || *p <= 0.0)) {
                let mut values: Vec<String> = allowed
                    .iter()
                    .take(5)
                    .map(|id| self.lookup(*id).map(|s| s.to_string()).unwrap_or_default())
                    .collect();
                if allowed.len() > 5 {
                    values.push("...".into());
                }
                let constraints: Vec<String> = fixed
                    .iter()
                    .map(|(k, v)| {
                        let id = *v.iter().next().unwrap();
                        format!(
                            "{}={}",
                            self.nodes[*k].name,
                            self.lookup(id).map(|s| s.to_string()).unwrap_or_default()
                        )
                    })
                    .collect();
                return Err(Error::RestrictiveConstraints(format!(
                    "Cannot generate fingerprint: {}=({}) is impossible with constraint: {}",
                    self.nodes[node].name,
                    values.join(", "),
                    constraints.join(", ")
                )));
            }
        }
        Ok(())
    }

    fn cpt_for<'a>(&'a self, node: &'a Node, parent_values: &[u32]) -> Option<&'a [(u32, f64)]> {
        let mut cpt = &node.cpt;
        for v in parent_values {
            match cpt {
                Cpt::Branch(map) => cpt = map.get(v)?,
                Cpt::Leaf(_) => return None,
            }
        }
        match cpt {
            Cpt::Leaf(leaf) if !leaf.is_empty() => Some(leaf),
            _ => None,
        }
    }

    /// The distribution of `target` given `evidence`, by beam search, in the
    /// order the values were first reached.
    fn trace_ids(&self, target: usize, evidence: &Evidence) -> Vec<(u32, f64)> {
        let n = self.nodes.len();
        let mut relevant = self.ancestors[target].clone();
        relevant[target] = true;
        for &ev in evidence.keys() {
            relevant[ev] = true;
            for (j, is) in self.ancestors[ev].iter().enumerate() {
                if *is {
                    relevant[j] = true;
                }
            }
        }
        let ordered: Vec<usize> = (0..n).filter(|i| relevant[*i]).collect();
        let mut position = vec![usize::MAX; n];
        for (pos, &i) in ordered.iter().enumerate() {
            position[i] = pos;
        }

        let mut beam: Vec<(Vec<u32>, f64)> = vec![(Vec::new(), 1.0)];
        let mut parent_values = Vec::new();
        for &ni in &ordered {
            let node = &self.nodes[ni];
            let allowed = evidence.get(&ni);
            let mut new_beam = Vec::new();
            for (assignment, prob) in &beam {
                parent_values.clear();
                parent_values.extend(node.parents.iter().map(|p| assignment[position[*p]]));
                let uniform;
                let cpt: &[(u32, f64)] = match self.cpt_for(node, &parent_values) {
                    Some(c) => c,
                    None if !node.possible.is_empty() => {
                        let p = 1.0 / node.possible.len() as f64;
                        uniform = node.possible.iter().map(|v| (*v, p)).collect::<Vec<_>>();
                        &uniform
                    }
                    None => &[],
                };
                for &(value, p) in cpt {
                    if p > 0.0 && allowed.is_none_or(|a| a.contains(&value)) {
                        let mut next = Vec::with_capacity(assignment.len() + 1);
                        next.extend_from_slice(assignment);
                        next.push(value);
                        new_beam.push((next, prob * p));
                    }
                }
            }
            if new_beam.is_empty() {
                return Vec::new();
            }
            if new_beam.len() > BEAM_WIDTH {
                // heapq.nlargest: the same as a stable descending sort, truncated.
                new_beam.sort_by(|a, b| b.1.partial_cmp(&a.1).unwrap_or(std::cmp::Ordering::Equal));
                new_beam.truncate(BEAM_WIDTH);
            }
            beam = new_beam;
        }

        let tpos = position[target];
        let mut order: Vec<u32> = Vec::new();
        let mut sums: HashMap<u32, f64> = HashMap::new();
        let mut total = 0.0;
        for (assignment, prob) in &beam {
            let value = assignment[tpos];
            let entry = sums.entry(value).or_insert_with(|| {
                order.push(value);
                0.0
            });
            *entry += prob;
            total += prob;
        }
        if total <= 0.0 {
            return Vec::new();
        }
        order.into_iter().map(|v| (v, sums[&v] / total)).collect()
    }

    fn sample(distribution: &[(u32, f64)]) -> u32 {
        let anchor: f64 = rand::random();
        let mut cumulative = 0.0;
        for &(value, p) in distribution {
            cumulative += p;
            if anchor < cumulative {
                return value;
            }
        }
        distribution[0].0
    }

    /// One full sample consistent with the evidence: a value id per node.
    fn generate_consistent_sample(&self, evidence: &Evidence) -> Result<Vec<u32>> {
        let mut result = Vec::with_capacity(self.nodes.len());
        let mut current = evidence.clone();
        for i in 0..self.nodes.len() {
            let sampled = if let Some(allowed) = current.get(&i).cloned() {
                let search: Evidence = current.iter().filter(|(k, _)| **k != i).map(|(k, v)| (*k, v.clone())).collect();
                let dist = self.trace_ids(i, &search);
                let mut filtered: Vec<(u32, f64)> = dist.into_iter().filter(|(v, _)| allowed.contains(v)).collect();
                let total: f64 = filtered.iter().map(|(_, p)| p).sum();
                if filtered.is_empty() || total <= 0.0 {
                    let p = 1.0 / allowed.len() as f64;
                    filtered = allowed.iter().map(|v| (*v, p)).collect();
                } else {
                    for e in &mut filtered {
                        e.1 /= total;
                    }
                }
                Self::sample(&filtered)
            } else {
                let dist = self.trace_ids(i, &current);
                if dist.is_empty() {
                    return Err(Error::RestrictiveConstraints(format!(
                        "Cannot generate fingerprint: empty distribution for {}.",
                        self.nodes[i].name
                    )));
                }
                Self::sample(&dist)
            };
            result.push(sampled);
            current.insert(i, HashSet::from([sampled]));
        }
        Ok(result)
    }

    /// Generate a fingerprint satisfying the conditions, as a nested object.
    pub fn generate(&self, conditions: &[(&str, Condition)]) -> Result<Value> {
        let mut evidence = Evidence::new();
        self.build_evidence(conditions, &mut evidence)?;
        let sample = self.generate_consistent_sample(&evidence)?;
        let mut out = Map::new();
        for (i, id) in sample.into_iter().enumerate() {
            let value = self.lookup_value(id)?;
            insert_path(&mut out, &self.nodes[i].name, value);
        }
        Ok(Value::Object(out))
    }

    /// The distribution of `target` given the conditions and extra evidence,
    /// most likely first.
    pub fn trace(&self, target: &str, conditions: &[(&str, Condition)], extra: &Evidence) -> Result<Vec<TraceResult>> {
        let mut evidence = extra.clone();
        self.build_evidence(conditions, &mut evidence)?;
        let node = self.node_index(target).ok_or_else(|| Error::InvalidNode(format!("Unknown node: \"{target}\"")))?;
        let dist = self.trace_ids(node, &evidence);
        if dist.is_empty() {
            return Err(Error::RestrictiveConstraints(format!(
                "Restraints are too restrictive. No possible values for {target}."
            )));
        }
        let mut out = dist
            .into_iter()
            .map(|(id, probability)| Ok(TraceResult { id, value: self.lookup_value(id)?, probability }))
            .collect::<Result<Vec<_>>>()?;
        out.sort_by(|a, b| b.probability.partial_cmp(&a.probability).unwrap_or(std::cmp::Ordering::Equal));
        Ok(out)
    }
}

fn at_path<'a>(value: &'a Value, path: &[String]) -> Option<&'a Value> {
    let mut v = value;
    for key in path {
        v = v.as_object()?.get(key)?;
    }
    Some(v)
}

fn insert_path(out: &mut Map<String, Value>, key: &str, value: Value) {
    let parts: Vec<&str> = key.split('.').collect();
    let mut cur = out;
    for part in &parts[..parts.len() - 1] {
        let entry = cur.entry(part.to_string()).or_insert_with(|| Value::Object(Map::new()));
        if !entry.is_object() {
            *entry = Value::Object(Map::new());
        }
        cur = entry.as_object_mut().unwrap();
    }
    cur.insert(parts[parts.len() - 1].to_string(), value);
}

static NETWORK: Mutex<Option<Arc<Network>>> = Mutex::new(None);

/// The network, loaded on first use (installing the pinned model if needed).
pub fn network() -> Result<Arc<Network>> {
    let mut guard = NETWORK.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(n) = guard.as_ref() {
        return Ok(n.clone());
    }
    let dir = model::ensure_model(false)?;
    let n = Arc::new(Network::load(&dir)?);
    *guard = Some(n.clone());
    Ok(n)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn base85_matches_python() {
        // base64.b85decode, read big-endian, for indices seen in the model.
        assert_eq!(base85_to_int("!"), 0);
        assert_eq!(base85_to_int("0R"), 1);
        assert_eq!(base85_to_int("1^"), 6);
        assert_eq!(base85_to_int("S^"), 90);
        assert_eq!(base85_to_int("jQ"), 141);
        assert_eq!(base85_to_int("0VV"), 294);
        assert_eq!(base85_to_int("0<i"), 689);
    }
}
