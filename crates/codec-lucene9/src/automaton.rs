pub const DEAD: u32 = u32::MAX;

pub struct WildcardDfa {
    transitions: Vec<u32>,
    accept: Vec<bool>,
    num_states: usize,
}

impl WildcardDfa {
    pub fn compile(pattern: &[u8]) -> Self {
        // State 0 = start. Each non-'*' byte in pattern advances to a new state.
        // '*' adds self-loop on current state + epsilon to next state.
        //
        // Two-pass: first count states, then fill transitions.
        // States: one per position in pattern that is NOT '*', plus final.
        // Simplification: allocate pattern.len()+1 states (upper bound), use only what's needed.

        let max_states = pattern.len() + 1;
        let mut transitions = vec![DEAD; max_states * 256];
        let mut accept = vec![false; max_states];

        // Build NFA-like structure then determinize inline.
        // For wildcard patterns the DFA is simple: track a set of "active positions".
        // Position i means "matched pattern[0..i] so far".
        // '*' at position i means position i loops and also epsilon-advances to i+1.
        //
        // Direct DFA construction: state = set of NFA positions (bitmask).
        // Since pattern.len() <= ~64 in practice, use u64 bitmask for NFA state sets.

        let n = pattern.len();
        if n == 0 {
            // Empty pattern: only accepts empty string
            accept[0] = true;
            return WildcardDfa { transitions, accept, num_states: 1 };
        }

        // NFA positions: 0..=n. Position n = accept.
        // epsilon closure: if position i has pattern[i] == '*', then i epsilon-transitions to i+1.
        fn epsilon_closure(pattern: &[u8], mut positions: u64) -> u64 {
            let n = pattern.len();
            loop {
                let mut new_positions = positions;
                for i in 0..n {
                    if positions & (1u64 << i) != 0 && pattern[i] == b'*' {
                        new_positions |= 1u64 << (i + 1);
                    }
                }
                if new_positions == positions {
                    break;
                }
                positions = new_positions;
            }
            positions
        }

        // BFS over DFA states (each DFA state = epsilon-closed NFA position set)
        use std::collections::HashMap;
        let mut state_map: HashMap<u64, u32> = HashMap::new();
        let mut queue: Vec<(u32, u64)> = Vec::new(); // (dfa_state_id, nfa_positions)

        let start_positions = epsilon_closure(pattern, 1); // NFA position 0 active
        state_map.insert(start_positions, 0);
        queue.push((0, start_positions));
        let mut num_states: usize = 1;

        while let Some((dfa_id, positions)) = queue.pop() {
            // Accept if NFA position n is in the set
            if positions & (1u64 << n) != 0 {
                accept[dfa_id as usize] = true;
            }

            // For each possible byte, compute next NFA position set
            for b in 0..256u32 {
                let byte = b as u8;
                let mut next_positions: u64 = 0;

                for i in 0..n {
                    if positions & (1u64 << i) == 0 {
                        continue;
                    }
                    match pattern[i] {
                        b'*' => {
                            // '*' matches any byte, stays at position i
                            next_positions |= 1u64 << i;
                        }
                        b'?' => {
                            // '?' matches any single byte, advances to i+1
                            next_positions |= 1u64 << (i + 1);
                        }
                        c => {
                            if c == byte {
                                next_positions |= 1u64 << (i + 1);
                            }
                        }
                    }
                }

                if next_positions == 0 {
                    transitions[dfa_id as usize * 256 + byte as usize] = DEAD;
                    continue;
                }

                next_positions = epsilon_closure(pattern, next_positions);

                let next_id = if let Some(&id) = state_map.get(&next_positions) {
                    id
                } else {
                    let id = num_states as u32;
                    num_states += 1;
                    state_map.insert(next_positions, id);
                    queue.push((id, next_positions));
                    id
                };
                transitions[dfa_id as usize * 256 + byte as usize] = next_id;
            }
        }

        transitions.truncate(num_states * 256);
        accept.truncate(num_states);
        WildcardDfa { transitions, accept, num_states }
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
