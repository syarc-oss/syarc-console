use std::collections::HashSet;

fn main() {
    let mut board = vec![vec![String::new(); 15]; 15];
    // First move: "HOME" horizontally through center (7,7..7,10)
    let placed = vec![
        ('H', 7, 6), ('O', 7, 7), ('M', 7, 8), ('E', 7, 9),
    ];
    let words = syarc_wordgame::test::formed_words(&board, &to_placed(&placed)).expect("words");
    assert_eq!(words.len(), 1);
    assert_eq!(words[0].word, "HOME");
    // H(4)x1 O1 M3 E1 = 9; O on center DW => 4+1+3+1=9 x2 = 18
    let score = syarc_wordgame::test::score_words(&words);
    assert_eq!(score, 18, "center DW should double HOME");
    for (l, r, c) in &placed { board[*r][*c] = l.to_string(); }

    // Second move: vertical "OMEGA"? place M at (8,8) making "M" column cross "OM"?
    // Simpler: place "AT" at (7,10)=(A),(7,11)=(T) extending HOME -> HOMEAT? not a word.
    // Place vertical word crossing E: (8,9)='A',(9,9)='R' forms EAR? E is at (7,9).
    let placed2 = vec![('A', 8, 9), ('R', 9, 9)];
    let words2 = syarc_wordgame::test::formed_words(&board, &to_placed(&placed2)).expect("words2");
    let names: Vec<&str> = words2.iter().map(|w| w.word.as_str()).collect();
    assert!(names.contains(&"EAR"), "expected EAR in {:?}", names);
    assert!(syarc_wordgame::test::dictionary().contains("EAR"));
    let s2 = syarc_wordgame::test::score_words(&words2);
    // EAR: E(1, existing) + A(1) + R(1x3 TL at 9,9) = 5
    assert_eq!(s2, 5);

    // dictionary sanity
    for w in ["QI", "WORD", "CHUMS"] {
        let hit = syarc_wordgame::test::dictionary().contains(w);
        assert!(hit, "{} missing", w);
    }
    // bag: 98 tiles
    assert_eq!(syarc_wordgame::test::fresh_bag().len(), 98);
    println!("ALL TESTS PASS");
    let _ = HashSet::<String>::new();
}

fn to_placed(v: &[(char, usize, usize)]) -> Vec<syarc_wordgame::test::Placed> {
    v.iter().map(|(l, r, c)| syarc_wordgame::test::Placed { letter: *l, row: *r, col: *c }).collect()
}
