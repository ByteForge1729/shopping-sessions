//! Linked IDs: union-find over every valid `links/` line, each person named by the
//! byte-wise smallest of their IDs. IDs in no valid line are people on their own.

use std::collections::HashMap;
use std::path::Path;

use crate::csv;
use crate::data::{self, Bytes, FastMap};

/// user ID hash -> (person key, person name), for linked IDs only. A person key is
/// the hash of the person's name, so an unlinked ID's key is its own hash.
pub struct People {
    map: FastMap<(u64, Bytes)>,
}

impl People {
    #[inline]
    pub fn person(&self, user_hash: u64, user: Bytes) -> (u64, Bytes) {
        self.map.get(&user_hash).copied().unwrap_or((user_hash, user))
    }

    pub fn build(dir: &Path, threads: usize) -> People {
        let chunks = data::load_dir(dir, 1 << 20);
        let pairs = data::par_for(chunks.len(), threads, Vec::new, |out: &mut Vec<(Bytes, Bytes)>, i| {
            for line in csv::lines(chunks[i]) {
                if let Some([a, b]) = csv::fields::<2>(line) {
                    if !a.is_empty() && !b.is_empty() {
                        out.push((a, b));
                    }
                }
            }
        });
        let mut uf = UnionFind::default();
        for (a, b) in pairs.into_iter().flatten() {
            let (x, y) = (uf.id(a), uf.id(b));
            uf.union(x, y);
        }
        People { map: uf.people() }
    }
}

#[derive(Default)]
struct UnionFind {
    index: FastMap<u32>,
    names: Vec<Bytes>,
    parent: Vec<u32>,
}

impl UnionFind {
    fn id(&mut self, name: Bytes) -> u32 {
        let h = data::h64(name);
        let next = self.names.len() as u32;
        *self.index.entry(h).or_insert_with(|| {
            self.names.push(name);
            self.parent.push(next);
            next
        })
    }

    fn find(&mut self, mut x: u32) -> u32 {
        let mut root = x;
        while self.parent[root as usize] != root {
            root = self.parent[root as usize];
        }
        while self.parent[x as usize] != root {
            let up = self.parent[x as usize];
            self.parent[x as usize] = root;
            x = up;
        }
        root
    }

    fn union(&mut self, a: u32, b: u32) {
        let (ra, rb) = (self.find(a), self.find(b));
        if ra != rb {
            self.parent[ra.max(rb) as usize] = ra.min(rb);
        }
    }

    fn people(mut self) -> FastMap<(u64, Bytes)> {
        let n = self.names.len();
        let mut smallest: HashMap<u32, Bytes> = HashMap::new();
        let roots: Vec<u32> = (0..n as u32).map(|i| self.find(i)).collect();
        for (i, &root) in roots.iter().enumerate() {
            let name = self.names[i];
            let best = smallest.entry(root).or_insert(name);
            if name < *best {
                *best = name;
            }
        }
        let mut out = FastMap::default();
        out.reserve(n);
        for (i, &root) in roots.iter().enumerate() {
            let name = smallest[&root];
            out.insert(data::h64(self.names[i]), (data::h64(name), name));
        }
        out
    }
}
