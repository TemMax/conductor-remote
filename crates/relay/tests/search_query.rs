//! The query grammar: tokens, the FTS5 expression, and that every expression parses in FTS5.

use conductor_remote::search::query::{match_query, query_tokens};
use rusqlite::Connection;

fn mq(raw: &str) -> Option<String> {
    match_query(raw)
}

#[test]
fn tokens_are_lowercased_runs_of_letters_numbers_and_underscore() {
    assert_eq!(
        query_tokens("Foo-Bar_baz 42 Ünï"),
        ["foo", "bar_baz", "42", "ünï"]
    );
    assert!(query_tokens("- * : ( )").is_empty());
}

#[test]
fn nothing_searchable_is_none() {
    for raw in ["", "   ", "\"\"", "“”", "- * : ( )"] {
        assert_eq!(mq(raw), None, "{raw:?}");
    }
}

#[test]
fn a_single_word_is_quoted_with_prefix_from_three_characters() {
    assert_eq!(mq("lamp").as_deref(), Some("\"lamp\"*"));
    assert_eq!(mq("la").as_deref(), Some("\"la\""));
    assert_eq!(mq("lamp ").as_deref(), Some("\"lamp\""));
}

#[test]
fn prefix_counts_utf16_units() {
    // One astral character is two UTF-16 units, so two of them reach three.
    assert_eq!(
        mq("\u{1D7D8}\u{1D7D8}").as_deref(),
        Some("\"\u{1D7D8}\u{1D7D8}\"*")
    );
    assert_eq!(mq("\u{1D7D8}").as_deref(), Some("\"\u{1D7D8}\""));
}

#[test]
fn white_space_set_is_javascripts() {
    assert_eq!(mq("lamp\u{FEFF}").as_deref(), Some("\"lamp\""));
    assert_eq!(mq("lamp\u{0085}").as_deref(), Some("\"lamp\"*"));
}

#[test]
fn several_words_add_one_phrase_term_beside_the_or_tokens() {
    assert_eq!(
        mq("manual lamp ").as_deref(),
        Some("(\"manual lamp\" OR \"manual\" OR \"lamp\")")
    );
    assert_eq!(
        mq("may i run the").as_deref(),
        Some("(\"may i run the\"* OR \"may\" OR \"i\" OR \"run\" OR \"the\"*)")
    );
}

#[test]
fn a_quoted_phrase_is_required_and_loose_words_stay_or() {
    assert_eq!(
        mq("\"race condition\"").as_deref(),
        Some("\"race condition\"")
    );
    assert_eq!(
        mq("\"race condition\" parked").as_deref(),
        Some("\"race condition\" AND \"parked\"*")
    );
    assert_eq!(
        mq("fix \"race condition\" parked queue").as_deref(),
        Some("\"race condition\" AND (\"fix\" OR \"parked queue\"* OR \"parked\" OR \"queue\"*)")
    );
}

#[test]
fn curly_quotes_count() {
    assert_eq!(
        mq("“race condition” parked").as_deref(),
        Some("\"race condition\" AND \"parked\"*")
    );
}

#[test]
fn an_unclosed_quote_is_the_phrase_still_being_typed() {
    assert_eq!(mq("\"may i run").as_deref(), Some("\"may i run\"*"));
    assert_eq!(mq("\"may i ru").as_deref(), Some("\"may i ru\""));
}

#[test]
fn a_leading_closed_empty_quote_does_not_flip_which_segments_are_quoted() {
    assert_eq!(
        mq("\"\"may i run the").as_deref(),
        Some("(\"may i run the\"* OR \"may\" OR \"i\" OR \"run\" OR \"the\"*)")
    );
}

// ------------------------------------------------------------ against real FTS5

fn chunks() -> Connection {
    let db = Connection::open_in_memory().unwrap();
    db.execute_batch("CREATE VIRTUAL TABLE chunks USING fts5(body, tokenize='porter unicode61')")
        .unwrap();
    for body in [
        "The controls are the correct local path. May I run the separate stop check?",
        "Running the headless artifact render. The first run may do a full import, and the run may repeat.",
        "The build is still running. The CI run may not have triggered, and the retry may run the same way.",
        "A parked prompt survives a race condition in the queue.",
    ] {
        db.execute("INSERT INTO chunks(body) VALUES (?1)", [body]).unwrap();
    }
    db
}

/// Bodies matching `raw`, best first; panics when FTS5 refuses the expression.
fn search(db: &Connection, raw: &str) -> Vec<String> {
    let Some(expr) = match_query(raw) else {
        return Vec::new();
    };
    let mut stmt = db
        .prepare("SELECT body FROM chunks WHERE chunks MATCH ?1 ORDER BY bm25(chunks)")
        .unwrap_or_else(|e| panic!("{raw:?} -> {expr}: {e}"));
    let rows = stmt
        .query_map([&expr], |r| r.get::<_, String>(0))
        .unwrap_or_else(|e| panic!("{raw:?} -> {expr}: {e}"));
    rows.map(|r| r.unwrap_or_else(|e| panic!("{raw:?} -> {expr}: {e}")))
        .collect()
}

#[test]
fn every_expression_parses_hostile_input_included() {
    let db = chunks();
    for raw in [
        "may i run the",
        "\"may i run the",
        "\"\"may i run the",
        "“may i run the”",
        "can't fix the drawer",
        "NEAR AND OR NOT",
        "NEAR(a b)",
        "NEAR/2 foo",
        "foo* -bar :baz (qux",
        "body:foo ^bar",
        "{body}: foo \"x\"",
        "a \"b\" c \"d e\" f",
        "\"unterminated",
        "))) ((( ***",
        "\"a\" \"b\" \"c",
        "🙂 \"🙂 ok\"",
    ] {
        search(&db, raw);
    }
}

#[test]
fn the_exact_sentence_outranks_the_chunks_that_merely_use_its_words() {
    let db = chunks();
    assert!(search(&db, "may i run the")[0].contains("May I run the separate stop check"));
}

#[test]
fn a_quoted_phrase_drops_every_chunk_that_lacks_it() {
    let db = chunks();
    assert_eq!(search(&db, "\"may i run the\" ").len(), 1);
    assert_eq!(search(&db, "\"race condition\" parked").len(), 1);
}

#[test]
fn stemming_still_applies_inside_quotes() {
    let db = chunks();
    assert_eq!(search(&db, "\"running the headless\" ").len(), 1);
}
