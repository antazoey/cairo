use cairo_lang_proc_macros::query_group;
use expect_test::expect;

mod logger_db;
use logger_db::LoggerDb;

#[salsa::interned(no_lifetime)]
pub struct InternedString {
    data: String,
}

#[query_group]
pub trait InternedDB: salsa::Database {
    #[salsa::interned]
    fn intern_string(&self, data: String) -> InternedString;

    fn interned_len(&self, id: InternedString) -> usize;
}

fn interned_len(db: &dyn InternedDB, id: InternedString) -> usize {
    db.lookup_intern_string(id).len()
}

#[test]
fn intern_round_trip() {
    let db = LoggerDb::default();

    let id = db.intern_string(String::from("Hello, world!"));
    let s = db.lookup_intern_string(id);

    assert_eq!(s.len(), 13);
    db.assert_logs(expect![[r#"[]"#]]);
}

#[test]
fn intern_with_query() {
    let db = LoggerDb::default();

    let id = db.intern_string(String::from("Hello, world!"));
    let len = db.interned_len(id);

    assert_eq!(len, 13);
    db.assert_logs(expect![[r#"
        [
            "salsa_event(WillCheckCancellation)",
            "salsa_event(WillExecute { database_key: create_data_InternedDB(Id(400)) })",
            "salsa_event(WillCheckCancellation)",
            "salsa_event(WillExecute { database_key: interned_len_shim(Id(c00)) })",
        ]"#]]);
}
