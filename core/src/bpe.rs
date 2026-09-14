// Licensed to the Apache Software Foundation (ASF) under one
// or more contributor license agreements.  See the NOTICE file
// distributed with this work for additional information
// regarding copyright ownership.  The ASF licenses this file
// to you under the Apache License, Version 2.0 (the
// "License"); you may not use this file except in compliance
// with the License.  You may obtain a copy of the License at
//
//   http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing,
// software distributed under the License is distributed on an
// "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied.  See the License for the
// specific language governing permissions and limitations
// under the License.

use std::collections::HashMap;

const TOKEN_BASE: u8 = 0x80;
const MAX_RULES: usize = 128;
const PAIR_TABLE_SIZE: usize = 1 << 16;
const DENSE_PAIR_THRESHOLD: usize = 4_096;
const NO_NODE: u32 = u32::MAX;
const NO_PAIR_SLOT: u32 = u32::MAX;
const MAX_INCREMENTAL_NAME_LEN: usize = u8::MAX as usize + 1;
const PACKED_INDEX_LIMIT: usize = 1 << 24;

pub fn is_ascii_only(names: &[&[u8]]) -> bool {
    names.iter().all(|name| name.iter().all(|&b| b & 0x80 == 0))
}

pub fn build_vocabulary(names: &[&[u8]]) -> Vec<[u8; 2]> {
    build_vocabulary_and_encode(names).0
}

pub(crate) fn build_vocabulary_and_encode(names: &[&[u8]]) -> (Vec<[u8; 2]>, Vec<Vec<u8>>) {
    if should_use_incremental(names) {
        return IncrementalBpe::new(names).build();
    }

    build_vocabulary_and_encode_rescan(names)
}

fn should_use_incremental(names: &[&[u8]]) -> bool {
    let total_bytes = names
        .iter()
        .try_fold(0usize, |total, name| total.checked_add(name.len()));
    let total_pairs = names.iter().try_fold(0usize, |total, name| {
        total.checked_add(name.len().saturating_sub(1))
    });

    is_ascii_only(names)
        && total_pairs.is_some_and(|pairs| pairs >= DENSE_PAIR_THRESHOLD)
        && total_bytes.is_some_and(|bytes| bytes < PACKED_INDEX_LIMIT)
        && names
            .iter()
            .all(|name| name.len() <= MAX_INCREMENTAL_NAME_LEN)
}

fn build_vocabulary_and_encode_rescan(names: &[&[u8]]) -> (Vec<[u8; 2]>, Vec<Vec<u8>>) {
    let mut tokens: Vec<Vec<u8>> = names.iter().map(|name| name.to_vec()).collect();

    let mut rules = Vec::new();
    let use_dense_counter = tokens
        .iter()
        .map(|seq| seq.len().saturating_sub(1))
        .sum::<usize>()
        >= DENSE_PAIR_THRESHOLD;
    let mut pair_counts = use_dense_counter.then(|| vec![0u32; PAIR_TABLE_SIZE]);
    let mut touched_pairs = Vec::new();

    for _ in 0..MAX_RULES {
        let best = if let Some(pair_counts) = &mut pair_counts {
            most_frequent_pair_dense(&tokens, pair_counts, &mut touched_pairs)
        } else {
            most_frequent_pair_hashmap(&tokens)
        };
        match best {
            Some((left, right, count)) if count > 1 => {
                let new_token = TOKEN_BASE + rules.len() as u8;
                rules.push([left, right]);

                for seq in &mut tokens {
                    replace_pair(seq, left, right, new_token);
                }
            }
            _ => break,
        }
    }

    (rules, tokens)
}

#[derive(Clone, Copy)]
struct TokenNode(u32);

impl TokenNode {
    // A node stays within one field name, so 8-bit relative links are enough for
    // the guarded incremental path. The remaining bytes cache the current token
    // and its right neighbor token, keeping hot pair checks to one node load.
    #[inline(always)]
    fn new(prev: u32, token: u8) -> Self {
        let prev_gap = u8::from(prev != NO_NODE);
        Self((token as u32) << 16 | prev_gap as u32)
    }

    #[inline(always)]
    fn prev(self, index: u32) -> u32 {
        let gap = self.0 as u8;
        if gap == 0 {
            NO_NODE
        } else {
            index - gap as u32
        }
    }

    #[inline(always)]
    fn set_prev(&mut self, index: u32, prev: u32) {
        let gap = if prev == NO_NODE {
            0
        } else {
            u8::try_from(index - prev).expect("BPE name length checked before incremental build")
        };
        self.0 = (self.0 & !0xff) | gap as u32;
    }

    #[inline(always)]
    fn token(self) -> u8 {
        (self.0 >> 16) as u8
    }

    #[inline(always)]
    fn set_token(&mut self, token: u8) {
        self.0 = (self.0 & !(0xff << 16)) | (token as u32) << 16;
    }

    #[inline(always)]
    fn next(self, index: u32) -> u32 {
        let gap = (self.0 >> 8) as u8;
        if gap == 0 {
            NO_NODE
        } else {
            index + gap as u32
        }
    }

    #[inline(always)]
    fn set_next(&mut self, index: u32, next: u32, right_token: u8) {
        let gap = if next == NO_NODE {
            0
        } else {
            u8::try_from(next - index).expect("BPE name length checked before incremental build")
        };
        self.0 = (self.0 & 0x00ff_00ff) | (gap as u32) << 8 | (right_token as u32) << 24;
    }

    #[inline(always)]
    fn right_token(self) -> u8 {
        (self.0 >> 24) as u8
    }
}

#[derive(Default)]
struct PackedOccurrences(Vec<u8>);

impl PackedOccurrences {
    // The incremental path is limited to 24-bit global node indexes. Packing
    // occurrences keeps its transient memory close to the normal writer path.
    fn with_capacity(entries: usize) -> Self {
        Self(Vec::with_capacity(entries.saturating_mul(3)))
    }

    #[inline(always)]
    fn push(&mut self, index: u32) {
        debug_assert!((index as usize) < PACKED_INDEX_LIMIT);
        let bytes = index.to_le_bytes();
        self.0.extend_from_slice(&bytes[..3]);
    }

    #[inline(always)]
    fn decode(bytes: &[u8]) -> u32 {
        bytes[0] as u32 | (bytes[1] as u32) << 8 | (bytes[2] as u32) << 16
    }
}

struct IncrementalBpe {
    nodes: Vec<TokenNode>,
    heads: Vec<u32>,
    pair_counts: Box<[u32]>,
    pair_occurrences: Vec<PackedOccurrences>,
    pair_slots: Box<[u32]>,
    known_pairs: Vec<u16>,
}

impl IncrementalBpe {
    fn new(names: &[&[u8]]) -> Self {
        let total_bytes = names.iter().map(|name| name.len()).sum();
        let mut nodes = Vec::with_capacity(total_bytes);
        let mut heads = Vec::with_capacity(names.len());

        for name in names {
            let head = if name.is_empty() {
                NO_NODE
            } else {
                u32::try_from(nodes.len()).expect("BPE input checked before incremental build")
            };
            heads.push(head);

            let mut prev = NO_NODE;
            for &token in *name {
                let index =
                    u32::try_from(nodes.len()).expect("BPE input checked before incremental build");
                nodes.push(TokenNode::new(prev, token));
                if prev != NO_NODE {
                    nodes[prev as usize].set_next(prev, index, token);
                }
                prev = index;
            }
        }

        let mut pair_counts = vec![0u32; PAIR_TABLE_SIZE].into_boxed_slice();
        for (left, node) in nodes.iter().enumerate() {
            if node.next(left as u32) != NO_NODE {
                let pair = (node.token() as usize) << 8 | node.right_token() as usize;
                pair_counts[pair] += 1;
            }
        }

        let mut pair_occurrences = Vec::new();
        let mut pair_slots = vec![NO_PAIR_SLOT; PAIR_TABLE_SIZE].into_boxed_slice();
        let mut known_pairs = Vec::new();
        for (pair, &count) in pair_counts.iter().enumerate() {
            if count != 0 {
                pair_slots[pair] = pair_occurrences.len() as u32;
                pair_occurrences.push(PackedOccurrences::with_capacity(count as usize));
                known_pairs.push(pair as u16);
            }
        }
        for (left, node) in nodes.iter().enumerate() {
            if node.next(left as u32) != NO_NODE {
                let pair = (node.token() as usize) << 8 | node.right_token() as usize;
                pair_occurrences[pair_slots[pair] as usize].push(left as u32);
            }
        }

        Self {
            nodes,
            heads,
            pair_counts,
            pair_occurrences,
            pair_slots,
            known_pairs,
        }
    }

    fn build(mut self) -> (Vec<[u8; 2]>, Vec<Vec<u8>>) {
        let mut rules = Vec::new();

        for _ in 0..MAX_RULES {
            let Some((pair, count)) = self.most_frequent_pair() else {
                break;
            };
            if count <= 1 {
                break;
            }

            let left_token = (pair >> 8) as u8;
            let right_token = pair as u8;
            let new_token = TOKEN_BASE + rules.len() as u8;
            rules.push([left_token, right_token]);
            self.replace_pair(pair, new_token);
        }

        let encoded = self
            .heads
            .iter()
            .map(|&head| {
                let mut sequence = Vec::new();
                let mut current = head;
                while current != NO_NODE {
                    let node = self.nodes[current as usize];
                    sequence.push(node.token());
                    current = node.next(current);
                }
                sequence
            })
            .collect();
        (rules, encoded)
    }

    fn most_frequent_pair(&self) -> Option<(u16, u32)> {
        let mut best_pair = 0u16;
        let mut best_count = 0u32;
        for &pair in &self.known_pairs {
            let count = self.pair_counts[pair as usize];
            if count > best_count || (count == best_count && pair > best_pair) {
                best_pair = pair;
                best_count = count;
            }
        }
        (best_count != 0).then_some((best_pair, best_count))
    }

    fn replace_pair(&mut self, pair: u16, new_token: u8) {
        let slot = self.pair_slots[pair as usize] as usize;
        let occurrences = std::mem::take(&mut self.pair_occurrences[slot]);
        // Only (x, x) occurrences can overlap. All other replacements commute,
        // so they need one validity check but no sort or second scan.
        if (pair >> 8) as u8 == pair as u8 {
            let mut occurrences = occurrences
                .0
                .chunks_exact(3)
                .map(PackedOccurrences::decode)
                .collect::<Vec<_>>();
            occurrences.sort_unstable();
            occurrences.dedup();
            for left in occurrences {
                if self.edge_pair(left) == Some(pair) {
                    self.replace_at(left, pair, new_token);
                }
            }
        } else {
            for occurrence in occurrences.0.chunks_exact(3) {
                let left = PackedOccurrences::decode(occurrence);
                if self.edge_pair(left) == Some(pair) {
                    self.replace_at(left, pair, new_token);
                }
            }
        }
        debug_assert_eq!(self.pair_counts[pair as usize], 0);
    }

    #[inline(always)]
    fn replace_at(&mut self, left: u32, pair: u16, new_token: u8) {
        let left_index = left as usize;
        let right = self.nodes[left_index].next(left);
        debug_assert_ne!(right, NO_NODE);
        let right_index = right as usize;
        let prev = self.nodes[left_index].prev(left);
        let next = self.nodes[right_index].next(right);
        let left_token = self.nodes[left_index].token();
        let right_token = self.nodes[right_index].token();
        let next_token = (next != NO_NODE).then(|| self.nodes[next as usize].token());

        if prev != NO_NODE {
            let prev_pair = (self.nodes[prev as usize].token() as usize) << 8 | left_token as usize;
            let count = &mut self.pair_counts[prev_pair];
            debug_assert!(*count > 0);
            *count -= 1;
        }
        let count = &mut self.pair_counts[pair as usize];
        debug_assert!(*count > 0);
        *count -= 1;
        if next != NO_NODE {
            let next_pair =
                (right_token as usize) << 8 | self.nodes[next as usize].token() as usize;
            let count = &mut self.pair_counts[next_pair];
            debug_assert!(*count > 0);
            *count -= 1;
        }

        self.nodes[left_index].set_token(new_token);
        self.nodes[left_index].set_next(left, next, next_token.unwrap_or_default());
        self.nodes[right_index].set_prev(right, NO_NODE);
        self.nodes[right_index].set_next(right, NO_NODE, 0);
        if next != NO_NODE {
            self.nodes[next as usize].set_prev(next, left);
        }

        if prev != NO_NODE {
            self.nodes[prev as usize].set_next(prev, left, new_token);
            let new_prev_pair = (self.nodes[prev as usize].token() as u16) << 8 | new_token as u16;
            self.add_occurrence(new_prev_pair, prev);
        }
        if let Some(next_token) = next_token {
            let new_next_pair = (new_token as u16) << 8 | next_token as u16;
            self.add_occurrence(new_next_pair, left);
        }
    }

    #[inline(always)]
    fn edge_pair(&self, left: u32) -> Option<u16> {
        if left == NO_NODE {
            return None;
        }
        let node = self.nodes[left as usize];
        if node.next(left) == NO_NODE {
            return None;
        }
        Some((node.token() as u16) << 8 | node.right_token() as u16)
    }

    #[inline(always)]
    fn add_occurrence(&mut self, pair: u16, left: u32) {
        let pair_index = pair as usize;
        let mut slot = self.pair_slots[pair_index];
        if slot == NO_PAIR_SLOT {
            slot = self.pair_occurrences.len() as u32;
            self.pair_slots[pair_index] = slot;
            self.pair_occurrences
                .push(PackedOccurrences::with_capacity(8));
            self.known_pairs.push(pair);
        }
        self.pair_counts[pair_index] += 1;
        self.pair_occurrences[slot as usize].push(left);
    }
}

fn most_frequent_pair_dense(
    tokens: &[Vec<u8>],
    pair_counts: &mut [u32],
    touched_pairs: &mut Vec<u16>,
) -> Option<(u8, u8, u32)> {
    debug_assert_eq!(pair_counts.len(), PAIR_TABLE_SIZE);
    for pair in touched_pairs.drain(..) {
        pair_counts[pair as usize] = 0;
    }

    let mut best_pair = 0usize;
    let mut best_count = 0u32;
    for seq in tokens {
        for pair in seq.windows(2) {
            let pair = ((pair[0] as usize) << 8) | pair[1] as usize;
            if pair_counts[pair] == 0 {
                touched_pairs.push(pair as u16);
            }
            pair_counts[pair] += 1;
            let count = pair_counts[pair];
            if count > best_count || (count == best_count && pair > best_pair) {
                best_pair = pair;
                best_count = count;
            }
        }
    }

    (best_count != 0).then_some(((best_pair >> 8) as u8, (best_pair & 0xff) as u8, best_count))
}

fn most_frequent_pair_hashmap(tokens: &[Vec<u8>]) -> Option<(u8, u8, u32)> {
    let mut pair_counts: HashMap<u16, u32> = HashMap::new();
    for seq in tokens {
        for pair in seq.windows(2) {
            let pair = (pair[0] as u16) << 8 | pair[1] as u16;
            *pair_counts.entry(pair).or_default() += 1;
        }
    }

    pair_counts
        .into_iter()
        .max_by_key(|&(pair, count)| (count, pair))
        .map(|(pair, count)| ((pair >> 8) as u8, pair as u8, count))
}

fn replace_pair(seq: &mut Vec<u8>, left: u8, right: u8, new_token: u8) {
    let mut i = 0;
    let mut out = 0;
    while i < seq.len() {
        if i + 1 < seq.len() && seq[i] == left && seq[i + 1] == right {
            seq[out] = new_token;
            i += 2;
        } else {
            seq[out] = seq[i];
            i += 1;
        }
        out += 1;
    }
    seq.truncate(out);
}

pub fn encode(name: &[u8], rules: &[[u8; 2]]) -> Vec<u8> {
    let mut tokens = name.to_vec();

    for (r, rule) in rules.iter().enumerate() {
        let left = rule[0];
        let right = rule[1];
        let new_token = TOKEN_BASE + r as u8;
        replace_pair(&mut tokens, left, right, new_token);
    }

    tokens
}

pub fn decode(encoded: &[u8], rules: &[[u8; 2]]) -> Vec<u8> {
    let mut out = Vec::with_capacity(encoded.len() * 2);
    for &b in encoded {
        expand(b as usize, rules, &mut out);
    }
    out
}

fn expand(token: usize, rules: &[[u8; 2]], out: &mut Vec<u8>) {
    if token < TOKEN_BASE as usize {
        out.push(token as u8);
    } else {
        let idx = token - TOKEN_BASE as usize;
        expand(rules[idx][0] as usize, rules, out);
        expand(rules[idx][1] as usize, rules, out);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reference_replace_pair(seq: &mut Vec<u16>, left: u16, right: u16, new_token: u16) {
        let mut i = 0;
        let mut out = 0;
        while i < seq.len() {
            if i + 1 < seq.len() && seq[i] == left && seq[i + 1] == right {
                seq[out] = new_token;
                i += 2;
            } else {
                seq[out] = seq[i];
                i += 1;
            }
            out += 1;
        }
        seq.truncate(out);
    }

    fn reference_build_vocabulary(names: &[&[u8]]) -> Vec<[u8; 2]> {
        let mut tokens: Vec<Vec<u16>> = names
            .iter()
            .map(|name| name.iter().map(|&b| b as u16).collect())
            .collect();
        let mut rules = Vec::new();

        for _ in 0..MAX_RULES {
            let mut pair_counts: HashMap<u32, u32> = HashMap::new();
            for seq in &tokens {
                for pair in seq.windows(2) {
                    let pair = (pair[0] as u32) << 16 | pair[1] as u32;
                    *pair_counts.entry(pair).or_default() += 1;
                }
            }

            match pair_counts
                .iter()
                .max_by_key(|&(&pair, &count)| (count, pair))
            {
                Some((&pair, &count)) if count > 1 => {
                    let left = (pair >> 16) as u16;
                    let right = pair as u16;
                    let new_token = TOKEN_BASE as u16 + rules.len() as u16;
                    rules.push([left as u8, right as u8]);
                    for seq in &mut tokens {
                        reference_replace_pair(seq, left, right, new_token);
                    }
                }
                _ => break,
            }
        }

        rules
    }

    #[test]
    fn test_bpe_encode() {
        let names: Vec<&[u8]> = vec![
            b"engine_coolant_temp",
            b"engine_coolant_pressure",
            b"engine_oil_temp",
            b"engine_oil_pressure",
        ];
        let rules = build_vocabulary(&names);
        assert!(!rules.is_empty());

        for &name in &names {
            let encoded = encode(name, &rules);
            assert!(encoded.len() <= name.len());
        }
    }

    #[test]
    fn test_build_vocabulary_and_encode_matches_separate_encoding() {
        let mut owned_names = vec![b"aaaaa".to_vec()];
        owned_names.extend((0..128).map(|i| {
            format!(
                "vehicle_data_collection_powertrain_group_{:03}_signal_measurement_{i:05}_value",
                i % 17
            )
            .into_bytes()
        }));
        let names: Vec<&[u8]> = owned_names.iter().map(Vec::as_slice).collect();

        let (rules, encoded) = build_vocabulary_and_encode(&names);
        assert_eq!(rules, reference_build_vocabulary(&names));
        assert_eq!(
            encoded,
            names
                .iter()
                .map(|name| encode(name, &rules))
                .collect::<Vec<_>>()
        );
    }

    #[test]
    fn test_incremental_bpe_matches_rescan_for_overlapping_pairs() {
        let patterns: [&[u8]; 8] = [
            b"aaaaa",
            b"abababa",
            b"abcabcabc",
            b"zzzzzzzz",
            b"vehicle_vehicle_vehicle",
            b"signal_signal_value",
            b"001001001001",
            b"",
        ];
        let owned_names = (0..1_024)
            .map(|i| patterns[i % patterns.len()].to_vec())
            .collect::<Vec<_>>();
        let names = owned_names.iter().map(Vec::as_slice).collect::<Vec<_>>();

        assert!(should_use_incremental(&names));
        assert_eq!(
            IncrementalBpe::new(&names).build(),
            build_vocabulary_and_encode_rescan(&names)
        );
    }

    #[test]
    fn test_incremental_bpe_handles_packed_index_and_name_length_boundaries() {
        let wide_names = (0..1_024)
            .map(|i| {
                format!(
                    "vehicle_data_collection_powertrain_group_{:03}_controller_{:02}_signal_{i:05}",
                    i % 173,
                    i % 29
                )
                .into_bytes()
            })
            .collect::<Vec<_>>();
        assert!(wide_names.iter().map(Vec::len).sum::<usize>() > u16::MAX as usize);
        let wide_refs = wide_names.iter().map(Vec::as_slice).collect::<Vec<_>>();
        assert!(should_use_incremental(&wide_refs));
        assert_eq!(
            IncrementalBpe::new(&wide_refs).build(),
            build_vocabulary_and_encode_rescan(&wide_refs)
        );

        let max_length_names =
            vec![vec![b'a'; MAX_INCREMENTAL_NAME_LEN]; DENSE_PAIR_THRESHOLD / 255 + 1];
        let max_length_refs = max_length_names
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        assert!(should_use_incremental(&max_length_refs));
        assert_eq!(
            IncrementalBpe::new(&max_length_refs).build(),
            build_vocabulary_and_encode_rescan(&max_length_refs)
        );

        let over_limit_names =
            vec![vec![b'a'; MAX_INCREMENTAL_NAME_LEN + 1]; DENSE_PAIR_THRESHOLD / 256 + 1];
        let over_limit_refs = over_limit_names
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        assert!(!should_use_incremental(&over_limit_refs));
        assert_eq!(
            build_vocabulary_and_encode(&over_limit_refs),
            build_vocabulary_and_encode_rescan(&over_limit_refs)
        );

        let non_ascii_names = vec![vec![0x80; 3]; DENSE_PAIR_THRESHOLD / 2];
        let non_ascii_refs = non_ascii_names
            .iter()
            .map(Vec::as_slice)
            .collect::<Vec<_>>();
        assert!(!should_use_incremental(&non_ascii_refs));
        assert_eq!(
            build_vocabulary_and_encode(&non_ascii_refs),
            build_vocabulary_and_encode_rescan(&non_ascii_refs)
        );
    }

    #[test]
    fn test_ascii_only() {
        assert!(is_ascii_only(&[b"hello", b"world"]));
        assert!(!is_ascii_only(&[b"hello", &[0x80, 0x81]]));
    }

    #[test]
    fn test_dense_pair_counter_reuses_storage_and_preserves_tie_breaking() {
        let mut counts = vec![0; 1 << 16];
        let mut touched = Vec::new();

        let first = vec![vec![1, 2, 1, 3]];
        assert_eq!(
            most_frequent_pair_dense(&first, &mut counts, &mut touched),
            Some((2, 1, 1))
        );

        let second = vec![vec![255, 254, 255, 254]];
        assert_eq!(
            most_frequent_pair_dense(&second, &mut counts, &mut touched),
            Some((255, 254, 2))
        );
    }

    #[test]
    fn test_dense_pair_counter_matches_hashmap_reference() {
        let mut state = 1u64;
        for _ in 0..32 {
            let mut names = Vec::new();
            for _ in 0..256 {
                let len = 4 + (state as usize % 28);
                let mut name = Vec::with_capacity(len);
                for _ in 0..len {
                    state = state
                        .wrapping_mul(6_364_136_223_846_793_005)
                        .wrapping_add(1);
                    name.push(b'a' + ((state >> 32) % 26) as u8);
                }
                names.push(name);
            }
            let refs: Vec<&[u8]> = names.iter().map(Vec::as_slice).collect();
            let (rules, encoded) = IncrementalBpe::new(&refs).build();
            assert_eq!(rules, reference_build_vocabulary(&refs));
            assert_eq!(
                encoded,
                refs.iter()
                    .map(|name| encode(name, &rules))
                    .collect::<Vec<_>>()
            );
            assert_eq!((rules, encoded), build_vocabulary_and_encode_rescan(&refs));
            if should_use_incremental(&refs) {
                assert_eq!(
                    build_vocabulary_and_encode(&refs),
                    build_vocabulary_and_encode_rescan(&refs)
                );
            }
        }
    }
}
