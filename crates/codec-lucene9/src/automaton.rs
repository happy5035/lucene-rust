pub const DEAD: u32 = u32::MAX;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct TransitionRange {
    pub min: u8,
    pub max: u8,
    pub dest: u32,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct WildcardDfa {
    transitions: Vec<u32>,
    accept: Vec<bool>,
    num_states: usize,
    ranges: Vec<Vec<TransitionRange>>,
    single: Option<Vec<u8>>,
}

impl WildcardDfa {
    pub fn compile(pattern: &[u8]) -> Self {
        // Range-based NFA + sweep-line subset construction.
        // NFA transitions are (min_byte, max_byte, target) ranges + epsilon.
        // This avoids per-byte enumeration: '?' = ~10 ranges, '*' = 1 range.

        struct RangeNfa {
            // ranges[state] = sorted Vec<(min, max, target)>
            ranges: Vec<Vec<(u8, u8, u32)>>,
            // eps[state] = epsilon targets
            eps: Vec<Vec<u32>>,
        }

        impl RangeNfa {
            fn new() -> Self {
                RangeNfa { ranges: vec![Vec::new()], eps: vec![Vec::new()] }
            }
            fn new_state(&mut self) -> u32 {
                let id = self.ranges.len() as u32;
                self.ranges.push(Vec::new());
                self.eps.push(Vec::new());
                id
            }
            fn add_range(&mut self, from: u32, min: u8, max: u8, to: u32) {
                self.ranges[from as usize].push((min, max, to));
            }
            fn add_epsilon(&mut self, from: u32, to: u32) {
                self.eps[from as usize].push(to);
            }
            fn add_utf8_any(&mut self, from: u32) -> u32 {
                let exit = self.new_state();
                // 1-byte: [00-7F]
                self.add_range(from, 0x00, 0x7F, exit);
                // 2-byte: [C2-DF][80-BF]
                let c2 = self.new_state();
                self.add_range(from, 0xC2, 0xDF, c2);
                self.add_range(c2, 0x80, 0xBF, exit);
                // 3-byte: [E0-EF][80-BF][80-BF]
                let c3a = self.new_state();
                let c3b = self.new_state();
                self.add_range(from, 0xE0, 0xEF, c3a);
                self.add_range(c3a, 0x80, 0xBF, c3b);
                self.add_range(c3b, 0x80, 0xBF, exit);
                // 4-byte: [F0-F4][80-BF][80-BF][80-BF]
                let c4a = self.new_state();
                let c4b = self.new_state();
                let c4c = self.new_state();
                self.add_range(from, 0xF0, 0xF4, c4a);
                self.add_range(c4a, 0x80, 0xBF, c4b);
                self.add_range(c4b, 0x80, 0xBF, c4c);
                self.add_range(c4c, 0x80, 0xBF, exit);
                exit
            }
        }

        let chars: Vec<char> = match std::str::from_utf8(pattern) {
            Ok(s) => s.chars().collect(),
            Err(_) => pattern.iter().map(|&b| b as char).collect(),
        };

        let mut nfa = RangeNfa::new();
        let start = 0u32;
        let mut current = start;

        for &ch in &chars {
            match ch {
                '*' => {
                    let next = nfa.new_state();
                    nfa.add_epsilon(current, next);
                    nfa.add_range(current, 0x00, 0xFF, current);
                    current = next;
                }
                '?' => {
                    current = nfa.add_utf8_any(current);
                }
                c => {
                    let mut buf = [0u8; 4];
                    let bytes = c.encode_utf8(&mut buf).as_bytes();
                    for &b in bytes {
                        let next = nfa.new_state();
                        nfa.add_range(current, b, b, next);
                        current = next;
                    }
                }
            }
        }

        let accept_state = current;
        let num_nfa = nfa.ranges.len();

        // Epsilon closure using a seen bitset (avoids Vec::contains O(n)).
        fn epsilon_closure(nfa: &RangeNfa, seeds: &[u32], num_nfa: usize) -> Vec<u32> {
            let mut seen = vec![false; num_nfa];
            let mut stack: Vec<u32> = seeds.to_vec();
            let mut result: Vec<u32> = Vec::new();
            for &s in &stack {
                seen[s as usize] = true;
            }
            while let Some(s) = stack.pop() {
                result.push(s);
                for &t in &nfa.eps[s as usize] {
                    if !seen[t as usize] {
                        seen[t as usize] = true;
                        stack.push(t);
                    }
                }
            }
            result.sort_unstable();
            result
        }

        // Subset construction with sweep-line alphabet partitioning.
        use std::collections::HashMap;

        let mut state_map: HashMap<Vec<u32>, u32> = HashMap::new();
        let mut queue: Vec<(u32, Vec<u32>)> = Vec::new();

        let start_set = epsilon_closure(&nfa, &[start], num_nfa);
        state_map.insert(start_set.clone(), 0);
        queue.push((0, start_set));

        let mut dfa_transitions: Vec<[u32; 256]> = vec![[DEAD; 256]];
        let mut dfa_accept: Vec<bool> = vec![false];
        let mut num_dfa: usize = 1;

        while let Some((dfa_id, nfa_set)) = queue.pop() {
            if nfa_set.contains(&accept_state) {
                dfa_accept[dfa_id as usize] = true;
            }

            // Collect all range transitions from active NFA states.
            let mut all_ranges: Vec<(u8, u8, u32)> = Vec::new();
            for &s in &nfa_set {
                all_ranges.extend_from_slice(&nfa.ranges[s as usize]);
            }
            if all_ranges.is_empty() {
                continue;
            }

            // Sweep-line: collect breakpoints (min and max+1 of each range),
            // then for each interval between consecutive breakpoints, compute
            // the set of targets active in that interval.
            let mut points: Vec<u16> = Vec::with_capacity(all_ranges.len() * 2 + 2);
            points.push(0);
            points.push(256);
            for &(min, max, _) in &all_ranges {
                points.push(min as u16);
                points.push(max as u16 + 1);
            }
            points.sort_unstable();
            points.dedup();

            // For each interval [points[i], points[i+1]), find active targets.
            let mut sig_cache: HashMap<Vec<u32>, u32> = HashMap::new();

            for w in points.windows(2) {
                let lo = w[0] as u8;
                let hi = (w[1] - 1) as u8; // inclusive end

                // Collect targets whose range overlaps [lo, hi]
                let mut targets: Vec<u32> = Vec::new();
                for &(min, max, target) in &all_ranges {
                    if min <= hi && max >= lo {
                        targets.push(target);
                    }
                }
                if targets.is_empty() {
                    continue;
                }
                targets.sort_unstable();
                targets.dedup();

                let next_id = if let Some(&id) = sig_cache.get(&targets) {
                    id
                } else {
                    let next_set = epsilon_closure(&nfa, &targets, num_nfa);
                    let id = if let Some(&id) = state_map.get(&next_set) {
                        id
                    } else {
                        let id = num_dfa as u32;
                        num_dfa += 1;
                        state_map.insert(next_set.clone(), id);
                        queue.push((id, next_set));
                        dfa_transitions.push([DEAD; 256]);
                        dfa_accept.push(false);
                        id
                    };
                    sig_cache.insert(targets, id);
                    id
                };

                // Fill flat table for all bytes in [lo, hi]
                for b in lo as usize..=hi as usize {
                    dfa_transitions[dfa_id as usize][b] = next_id;
                }
            }
        }

        // Flatten to the expected format
        let mut transitions = Vec::with_capacity(num_dfa * 256);
        for row in &dfa_transitions {
            transitions.extend_from_slice(row);
        }

        let ranges = Self::compute_ranges(&transitions, num_dfa);
        let single = Self::detect_single(&transitions, &dfa_accept, num_dfa);

        WildcardDfa { transitions, accept: dfa_accept, num_states: num_dfa, ranges, single }
    }

    pub fn start(&self) -> u32 {
        0
    }

    pub fn is_accept(&self, state: u32) -> bool {
        self.accept[state as usize]
    }

    pub fn transition(&self, state: u32, byte: u8) -> u32 {
        self.transitions[state as usize * 256 + byte as usize]
    }

    pub fn accepts(&self, input: &[u8]) -> bool {
        let mut state = self.start();
        for &b in input {
            state = self.transition(state, b);
            if state == DEAD {
                return false;
            }
        }
        self.is_accept(state)
    }

    pub fn ranges(&self, state: u32) -> &[TransitionRange] {
        &self.ranges[state as usize]
    }

    pub fn single_string(&self) -> Option<&[u8]> {
        self.single.as_deref()
    }

    /// Longest prefix where the DFA has exactly one single-byte transition
    /// per state. Returns (prefix_bytes, dfa_state_after_prefix).
    pub fn deterministic_prefix(&self) -> (Vec<u8>, u32) {
        let mut prefix = Vec::new();
        let mut state = 0u32;
        loop {
            let ranges = &self.ranges[state as usize];
            if ranges.len() == 1 && ranges[0].min == ranges[0].max {
                prefix.push(ranges[0].min);
                state = ranges[0].dest;
            } else {
                break;
            }
        }
        (prefix, state)
    }

    fn compute_ranges(transitions: &[u32], num_states: usize) -> Vec<Vec<TransitionRange>> {
        let mut ranges = Vec::with_capacity(num_states);
        for s in 0..num_states {
            let row = &transitions[s * 256..(s + 1) * 256];
            let mut state_ranges: Vec<TransitionRange> = Vec::new();
            let mut i = 0usize;
            while i < 256 {
                let dest = row[i];
                if dest == DEAD {
                    i += 1;
                    continue;
                }
                let min = i as u8;
                while i < 256 && row[i] == dest {
                    i += 1;
                }
                let max = (i - 1) as u8;
                state_ranges.push(TransitionRange { min, max, dest });
            }
            ranges.push(state_ranges);
        }
        ranges
    }

    fn detect_single(transitions: &[u32], accept: &[bool], num_states: usize) -> Option<Vec<u8>> {
        let mut state = 0u32;
        let mut out = Vec::new();
        let mut visited = vec![false; num_states];
        loop {
            if visited[state as usize] {
                return None;
            }
            visited[state as usize] = true;
            if accept[state as usize] {
                let row = &transitions[state as usize * 256..(state as usize + 1) * 256];
                if row.iter().any(|&d| d != DEAD) {
                    return None;
                }
                return Some(out);
            }
            let row = &transitions[state as usize * 256..(state as usize + 1) * 256];
            let mut found: Option<(u8, u32)> = None;
            for (b, &d) in row.iter().enumerate() {
                if d != DEAD {
                    if found.is_some() {
                        return None;
                    }
                    found = Some((b as u8, d));
                }
            }
            let (b, next) = found?;
            out.push(b);
            state = next;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pure_prefix() {
        let dfa = WildcardDfa::compile(b"foo*");
        assert!(dfa.accepts(b"foo"));
        assert!(dfa.accepts(b"foobar"));
        assert!(dfa.accepts(b"fooxyz"));
        assert!(!dfa.accepts(b"fo"));
        assert!(!dfa.accepts(b"xfoo"));
        assert!(!dfa.accepts(b""));
    }

    #[test]
    fn question_mark() {
        let dfa = WildcardDfa::compile(b"a?e");
        assert!(dfa.accepts(b"abe"));
        assert!(dfa.accepts(b"axe"));
        assert!(!dfa.accepts(b"ae"));
        assert!(!dfa.accepts(b"abbe"));
        assert!(!dfa.accepts(b"a"));
    }

    #[test]
    fn leading_star() {
        let dfa = WildcardDfa::compile(b"*foo");
        assert!(dfa.accepts(b"foo"));
        assert!(dfa.accepts(b"barfoo"));
        assert!(dfa.accepts(b"xfoo"));
        assert!(!dfa.accepts(b"foob"));
        assert!(!dfa.accepts(b"fo"));
    }

    #[test]
    fn star_in_middle() {
        let dfa = WildcardDfa::compile(b"a*b*c");
        assert!(dfa.accepts(b"abc"));
        assert!(dfa.accepts(b"aXbYc"));
        assert!(dfa.accepts(b"aXXXbYYYc"));
        assert!(!dfa.accepts(b"acb"));
        assert!(!dfa.accepts(b"ab"));
        assert!(!dfa.accepts(b"bc"));
    }

    #[test]
    fn star_only() {
        let dfa = WildcardDfa::compile(b"*");
        assert!(dfa.accepts(b""));
        assert!(dfa.accepts(b"anything"));
        assert!(dfa.accepts(b"x"));
    }

    #[test]
    fn consecutive_stars() {
        let dfa = WildcardDfa::compile(b"a**b");
        assert!(dfa.accepts(b"ab"));
        assert!(dfa.accepts(b"aXXb"));
        assert!(!dfa.accepts(b"a"));
        assert!(!dfa.accepts(b"b"));
    }

    #[test]
    fn exact_no_wildcards() {
        let dfa = WildcardDfa::compile(b"abc");
        assert!(dfa.accepts(b"abc"));
        assert!(!dfa.accepts(b"abd"));
        assert!(!dfa.accepts(b"ab"));
        assert!(!dfa.accepts(b"abcd"));
    }

    #[test]
    fn trailing_question() {
        let dfa = WildcardDfa::compile(b"test?");
        assert!(dfa.accepts(b"tests"));
        assert!(dfa.accepts(b"test1"));
        assert!(!dfa.accepts(b"test"));
        assert!(!dfa.accepts(b"test12"));
    }

    #[test]
    fn empty_pattern() {
        let dfa = WildcardDfa::compile(b"");
        assert!(dfa.accepts(b""));
        assert!(!dfa.accepts(b"a"));
    }

    #[test]
    fn ranges_literal() {
        let dfa = WildcardDfa::compile(b"ab");
        let r0 = dfa.ranges(0);
        assert_eq!(r0.len(), 1);
        assert_eq!(r0[0], TransitionRange { min: b'a', max: b'a', dest: 1 });
        let r1 = dfa.ranges(1);
        assert_eq!(r1.len(), 1);
        assert_eq!(r1[0], TransitionRange { min: b'b', max: b'b', dest: 2 });
        assert!(dfa.ranges(2).is_empty());
    }

    #[test]
    fn ranges_star() {
        let dfa = WildcardDfa::compile(b"a*");
        let r0 = dfa.ranges(0);
        assert_eq!(r0.len(), 1);
        assert_eq!(r0[0].min, b'a');
        assert_eq!(r0[0].max, b'a');
        let accept_state = r0[0].dest;
        let ra = dfa.ranges(accept_state);
        assert_eq!(ra.len(), 1);
        assert_eq!(ra[0].min, 0x00);
        assert_eq!(ra[0].max, 0xFF);
    }

    #[test]
    fn single_literal() {
        let dfa = WildcardDfa::compile(b"hello");
        assert_eq!(dfa.single_string(), Some(b"hello".as_slice()));
    }

    #[test]
    fn single_empty() {
        let dfa = WildcardDfa::compile(b"");
        assert_eq!(dfa.single_string(), Some(b"".as_slice()));
    }

    #[test]
    fn single_none_for_star() {
        assert_eq!(WildcardDfa::compile(b"h*llo").single_string(), None);
        assert_eq!(WildcardDfa::compile(b"foo*").single_string(), None);
        assert_eq!(WildcardDfa::compile(b"*").single_string(), None);
    }

    #[test]
    fn single_none_for_question() {
        assert_eq!(WildcardDfa::compile(b"h?llo").single_string(), None);
        assert_eq!(WildcardDfa::compile(b"?").single_string(), None);
    }
}
