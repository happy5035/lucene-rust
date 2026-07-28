pub const DEAD: u32 = u32::MAX;

pub struct WildcardDfa {
    transitions: Vec<u32>,
    accept: Vec<bool>,
    num_states: usize,
}

impl WildcardDfa {
    pub fn compile(pattern: &[u8]) -> Self {
        // Build a byte-level DFA from a wildcard pattern with Unicode-aware '?'.
        // '?' matches exactly one Unicode code point (1-4 UTF-8 bytes).
        // '*' matches any sequence of bytes (zero or more).
        // Literal chars match their UTF-8 byte sequence.
        //
        // Strategy: expand the pattern into a byte-level NFA where '?' becomes
        // a sub-NFA accepting any valid UTF-8 code point, then determinize.

        // NFA representation: states numbered 0..num_nfa_states.
        // Transitions: for each state, a list of (byte_predicate, target_state).
        // We use a flat Vec of transitions grouped by state.
        // Epsilon transitions are handled via closure.

        // First, build the NFA as a list of "fragments" connected in sequence.
        // Each fragment has an entry state and an exit state.

        struct NfaBuilder {
            // transitions[state] = Vec<(Option<u8>, target)> where None = epsilon
            transitions: Vec<Vec<(Option<u8>, u32)>>,
        }

        impl NfaBuilder {
            fn new() -> Self {
                NfaBuilder { transitions: vec![Vec::new()] }
            }
            fn new_state(&mut self) -> u32 {
                let id = self.transitions.len() as u32;
                self.transitions.push(Vec::new());
                id
            }
            fn add_transition(&mut self, from: u32, byte: u8, to: u32) {
                self.transitions[from as usize].push((Some(byte), to));
            }
            fn add_epsilon(&mut self, from: u32, to: u32) {
                self.transitions[from as usize].push((None, to));
            }
            /// Add a sub-NFA matching any valid UTF-8 code point (1-4 bytes).
            /// Entry state is `from`, returns the exit state.
            fn add_utf8_any(&mut self, from: u32) -> u32 {
                let exit = self.new_state();
                // 1-byte: 0x00-0x7F
                for b in 0x00u8..=0x7F {
                    self.add_transition(from, b, exit);
                }
                // 2-byte: 0xC2-0xDF, 0x80-0xBF
                let cont = self.new_state();
                for b in 0x80u8..=0xBF {
                    self.add_transition(cont, b, exit);
                }
                for b in 0xC2u8..=0xDF {
                    self.add_transition(from, b, cont);
                }
                // 3-byte: 0xE0-0xEF, 0x80-0xBF, 0x80-0xBF
                let cont2 = self.new_state();
                for b in 0x80u8..=0xBF {
                    self.add_transition(cont2, b, exit);
                }
                let cont1_3 = self.new_state();
                for b in 0x80u8..=0xBF {
                    self.add_transition(cont1_3, b, cont2);
                }
                for b in 0xE0u8..=0xEF {
                    self.add_transition(from, b, cont1_3);
                }
                // 4-byte: 0xF0-0xF4, 0x80-0xBF, 0x80-0xBF, 0x80-0xBF
                let cont3 = self.new_state();
                for b in 0x80u8..=0xBF {
                    self.add_transition(cont3, b, exit);
                }
                let cont2_4 = self.new_state();
                for b in 0x80u8..=0xBF {
                    self.add_transition(cont2_4, b, cont3);
                }
                let cont1_4 = self.new_state();
                for b in 0x80u8..=0xBF {
                    self.add_transition(cont1_4, b, cont2_4);
                }
                for b in 0xF0u8..=0xF4 {
                    self.add_transition(from, b, cont1_4);
                }
                exit
            }
        }

        // Parse pattern as UTF-8 chars; fall back to byte-level if invalid.
        let chars: Vec<char> = match std::str::from_utf8(pattern) {
            Ok(s) => s.chars().collect(),
            Err(_) => pattern.iter().map(|&b| b as char).collect(),
        };

        let mut nfa = NfaBuilder::new();
        let start = 0u32;
        let mut current = start;

        for &ch in &chars {
            match ch {
                '*' => {
                    // '*' = self-loop on current state (matches any byte) + advance.
                    // Model: current --epsilon--> next, current --any_byte--> current
                    let next = nfa.new_state();
                    nfa.add_epsilon(current, next);
                    // Self-loop: any byte stays at current
                    for b in 0u8..=255 {
                        nfa.add_transition(current, b, current);
                    }
                    current = next;
                }
                '?' => {
                    // '?' matches exactly one Unicode code point
                    current = nfa.add_utf8_any(current);
                }
                c => {
                    // Literal: match UTF-8 bytes of c sequentially
                    let mut buf = [0u8; 4];
                    let bytes = c.encode_utf8(&mut buf).as_bytes();
                    for &b in bytes {
                        let next = nfa.new_state();
                        nfa.add_transition(current, b, next);
                        current = next;
                    }
                }
            }
        }

        let accept_state = current;

        // Determinize via subset construction (BFS over epsilon-closed NFA state sets).
        use std::collections::HashMap;

        fn epsilon_closure(nfa: &NfaBuilder, mut states: Vec<u32>) -> Vec<u32> {
            states.sort_unstable();
            states.dedup();
            let mut i = 0;
            while i < states.len() {
                let s = states[i];
                for &(byte_opt, target) in &nfa.transitions[s as usize] {
                    if byte_opt.is_none() && !states.contains(&target) {
                        states.push(target);
                    }
                }
                i += 1;
            }
            states.sort_unstable();
            states
        }

        // Use a Vec<u32> as the key for state sets (sorted, deduped).
        let mut state_map: HashMap<Vec<u32>, u32> = HashMap::new();
        let mut queue: Vec<(u32, Vec<u32>)> = Vec::new();

        let start_set = epsilon_closure(&nfa, vec![start]);
        state_map.insert(start_set.clone(), 0);
        queue.push((0, start_set));

        let mut dfa_transitions: Vec<[u32; 256]> = vec![[DEAD; 256]];
        let mut dfa_accept: Vec<bool> = vec![false];
        let mut num_dfa: usize = 1;

        while let Some((dfa_id, nfa_set)) = queue.pop() {
            if nfa_set.contains(&accept_state) {
                dfa_accept[dfa_id as usize] = true;
            }

            for b in 0..256u32 {
                let byte = b as u8;
                let mut next_set: Vec<u32> = Vec::new();

                for &s in &nfa_set {
                    for &(byte_opt, target) in &nfa.transitions[s as usize] {
                        if let Some(tb) = byte_opt {
                            if tb == byte {
                                next_set.push(target);
                            }
                        }
                    }
                }

                if next_set.is_empty() {
                    dfa_transitions[dfa_id as usize][byte as usize] = DEAD;
                    continue;
                }

                next_set = epsilon_closure(&nfa, next_set);

                let next_id = if let Some(&id) = state_map.get(&next_set) {
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
                dfa_transitions[dfa_id as usize][byte as usize] = next_id;
            }
        }

        // Flatten to the expected format
        let mut transitions = Vec::with_capacity(num_dfa * 256);
        for row in &dfa_transitions {
            transitions.extend_from_slice(row);
        }

        WildcardDfa { transitions, accept: dfa_accept, num_states: num_dfa }
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
}
