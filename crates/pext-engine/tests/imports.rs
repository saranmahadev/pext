mod common;
use common::H;
use pext_engine::ops::imports::clean_amount;
use serde_json::json;
use std::io::Write;

fn file(h: &H, name: &str, body: &str) -> String {
    let p = h.dir.path().join(name);
    std::fs::File::create(&p)
        .unwrap()
        .write_all(body.as_bytes())
        .unwrap();
    p.to_string_lossy().to_string()
}

#[test]
fn clean_amount_understands_bank_formats() {
    let ok = |s: &str, comma: bool| clean_amount(s, comma).unwrap();
    assert_eq!(ok("1,234.56", false), "1234.56");
    assert_eq!(ok("₹ 1,23,456.00", false), "123456.00");
    assert_eq!(ok("(450.50)", false), "-450.50");
    assert_eq!(ok("450.50 Dr", false), "-450.50");
    assert_eq!(ok("450.50CR", false), "450.50");
    assert_eq!(ok("-12", false), "-12");
    assert_eq!(ok("+12", false), "12");
    assert_eq!(ok("1.234,56", true), "1234.56");
    assert_eq!(ok("USD 5.00", false), "5.00");
    for bad in ["", "abc", "--5", "(5)-", "1.2.3", "(5) Dr", "12 34x"] {
        assert!(
            clean_amount(bad, false).is_err(),
            "{bad} should be rejected"
        );
    }
}

const STATEMENT: &str = "Date,Narration,Debit,Credit,Ref\n\
01/10/2026,UPI-SWIGGY-123,450.50,,R1\n\
02/10/2026,SALARY ACME,,85000.00,R2\n\
03/10/2026,ATM WDL,\"2,000.00\",,R3\n";

fn csv_args(f: &str) -> serde_json::Value {
    json!({"account": "Bank", "file": f, "date_col": "Date", "payee_col": "Narration", "debit_col": "Debit",
           "credit_col": "Credit", "ref_col": "Ref", "date_format": "%d/%m/%Y"})
}

#[test]
fn csv_import_dry_run_then_commit_then_dedupe() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let f = file(&h, "s.csv", STATEMENT);
    let before = h.dump();
    let d = h.dry("import.csv", csv_args(&f));
    assert_eq!(d.data["imported"], 3);
    assert_eq!(before, h.dump());

    let r = h.ok("import.csv", csv_args(&f));
    assert_eq!(r["imported"], 3);
    assert_eq!(r["money_in_minor"], 8_500_000);
    assert_eq!(r["money_out_minor"], 45_050 + 200_000);
    assert_eq!(h.balance("Bank"), 8_500_000 - 45_050 - 200_000);

    // re-import: everything is a duplicate
    let r2 = h.ok("import.csv", csv_args(&f));
    assert_eq!(r2["imported"], 0);
    assert_eq!(r2["duplicates_skipped"], 3);
    assert_eq!(h.ok("txn.list", json!({})).as_array().unwrap().len(), 3);
    h.doctor_ok();
}

#[test]
fn csv_without_reference_uses_fingerprint_and_keeps_genuine_repeats() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let body =
        "date,desc,amount\n2026-10-01,COFFEE,-4.50\n2026-10-01,COFFEE,-4.50\n2026-10-02,TEA,-3\n";
    let f = file(&h, "n.csv", body);
    let args = json!({"account": "Bank", "file": f, "date_col": "date", "payee_col": "desc", "amount_col": "amount"});
    assert_eq!(h.ok("import.csv", args.clone())["imported"], 3); // two identical coffees are both real
    let again = h.ok("import.csv", args.clone());
    assert_eq!(again["imported"], 0);
    assert_eq!(again["duplicates_skipped"], 3);
    // a later file containing the same two coffees plus a third imports only the new one
    let f2 = file(&h, "n2.csv", "date,desc,amount\n2026-10-01,COFFEE,-4.50\n2026-10-01,COFFEE,-4.50\n2026-10-01,COFFEE,-4.50\n");
    let a2 = json!({"account": "Bank", "file": f2, "date_col": "date", "payee_col": "desc", "amount_col": "amount"});
    assert_eq!(h.ok("import.csv", a2)["imported"], 1);
}

#[test]
fn bad_rows_abort_by_default_and_skip_on_request() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let f = file(
        &h,
        "b.csv",
        "date,desc,amount\n2026-10-01,A,-1\nnot-a-date,B,-2\n2026-10-03,C,abc\n2026-10-04,D,-4\n",
    );
    let args = json!({"account": "Bank", "file": f, "date_col": "date", "payee_col": "desc", "amount_col": "amount"});
    let before = h.dump();
    let e = h.err("import.csv", args.clone());
    assert_eq!(e.code, "import_errors");
    assert_eq!(e.details.unwrap()["total_errors"], 2);
    assert_eq!(before, h.dump(), "a failed import must write nothing");
    let mut skip = args;
    skip["on_error"] = json!("skip");
    let r = h.ok("import.csv", skip);
    assert_eq!(r["imported"], 2);
    assert_eq!(r["unreadable_rows"].as_array().unwrap().len(), 2);
}

#[test]
fn csv_options_invert_decimal_comma_delimiter_and_bom() {
    let h = H::new();
    h.acct("Card", "credit_card", "0");
    let f = file(
        &h,
        "c.csv",
        "\u{feff}Datum;Betrag;Text\n01.10.2026;12,50;Shop\n02.10.2026;1.000,00;Rent\n",
    );
    let r = h.ok("import.csv", json!({"account": "Card", "file": f, "date_col": "Datum", "amount_col": "Betrag", "payee_col": "Text",
        "delimiter": ";", "decimal_comma": true, "invert": true, "date_format": "%d.%m.%Y"}));
    assert_eq!(r["imported"], 2);
    assert_eq!(h.balance("Card"), -(100_000 + 1_250)); // purchases flipped to negative
}

#[test]
fn csv_errors_are_helpful() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let f = file(&h, "e.csv", "date,amount\n2026-10-01,-1\n");
    let e = h.err(
        "import.csv",
        json!({"account": "Bank", "file": f, "date_col": "nope", "amount_col": "amount"}),
    );
    assert_eq!(e.code, "column_not_found");
    assert!(e.details.unwrap()["columns"].as_array().unwrap().len() == 2);
    assert_eq!(
        h.err(
            "import.csv",
            json!({"account": "Bank", "file": f, "date_col": "date"})
        )
        .code,
        "missing_argument"
    );
    assert_eq!(
        h.err("import.csv", json!({"account": "Bank", "file": f, "date_col": "date", "amount_col": "amount", "debit_col": "amount"})).code,
        "conflicting_arguments"
    );
    assert_eq!(h.err("import.csv", json!({"account": "Bank", "file": "/nonexistent.csv", "date_col": "date", "amount_col": "amount"})).code, "file_not_found");
    let bin = h.dir.path().join("bin.csv");
    std::fs::write(&bin, [0xff, 0xfe, 0x00, 0x41]).unwrap();
    assert_eq!(h.err("import.csv", json!({"account": "Bank", "file": bin.to_string_lossy(), "date_col": "date", "amount_col": "amount"})).code, "invalid_encoding");
}

#[test]
fn profiles_are_saved_and_reused() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let f = file(&h, "s.csv", STATEMENT);
    let mut a = csv_args(&f);
    a["save_profile"] = json!("hdfc");
    h.ok("import.csv", a);
    assert_eq!(
        h.ok("import.profiles", json!({})).as_array().unwrap().len(),
        1
    );
    let f2 = file(
        &h,
        "s2.csv",
        "Date,Narration,Debit,Credit,Ref\n05/10/2026,NEW,10.00,,R9\n",
    );
    let r = h.ok(
        "import.csv",
        json!({"account": "Bank", "file": f2, "profile": "hdfc"}),
    );
    assert_eq!(r["imported"], 1);
    assert_eq!(
        h.err(
            "import.csv",
            json!({"account": "Bank", "file": f2, "profile": "zzz"})
        )
        .code,
        "profile_not_found"
    );
}

#[test]
fn rollback_removes_exactly_one_batch() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    h.txn("Bank", "-1", "2026-09-01", None);
    let f = file(&h, "s.csv", STATEMENT);
    let r = h.ok("import.csv", csv_args(&f));
    let b = r["batch_id"].as_i64().unwrap();
    assert_eq!(
        h.err("import.rollback", json!({"batch": b})).code,
        "confirmation_required"
    );
    let rb = h.yes("import.rollback", json!({"batch": b})).unwrap();
    assert_eq!(rb.data["transactions_removed"], 3);
    assert_eq!(h.balance("Bank"), -100);
    assert_eq!(
        h.yes("import.rollback", json!({"batch": b}))
            .unwrap_err()
            .code,
        "already_rolled_back"
    );
    assert_eq!(
        h.yes("import.rollback", json!({"batch": 99}))
            .unwrap_err()
            .kind
            .as_str(),
        "not_found"
    );
    // the same statement can be imported again after a rollback
    assert_eq!(h.ok("import.csv", csv_args(&f))["imported"], 3);
    // and a rollback itself can be undone
    h.doctor_ok();
}

const OFX: &str = "OFXHEADER:100\nDATA:OFXSGML\n\n<OFX><BANKMSGSRSV1><STMTTRNRS><STMTRS><CURDEF>INR\n<BANKTRANLIST>\n\
<STMTTRN><TRNTYPE>DEBIT<DTPOSTED>20261001120000<TRNAMT>-450.50<FITID>F1<NAME>SWIGGY &amp; CO<MEMO>order</STMTTRN>\n\
<STMTTRN><TRNTYPE>CREDIT<DTPOSTED>20261002<TRNAMT>85000.00<FITID>F2<NAME>ACME</STMTTRN>\n\
</BANKTRANLIST></STMTRS></STMTTRNRS></BANKMSGSRSV1></OFX>";

#[test]
fn ofx_import_parses_sgml_and_dedupes_by_fitid() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    let f = file(&h, "s.ofx", OFX);
    let r = h.ok("import.ofx", json!({"account": "Bank", "file": f}));
    assert_eq!(r["imported"], 2);
    let rows = h.ok("txn.list", json!({"sort": "date"}));
    assert_eq!(rows[0]["payee"], "SWIGGY & CO");
    assert_eq!(rows[0]["amount_minor"], -45050);
    assert_eq!(rows[0]["date"], "2026-10-01");
    let full = h.ok("txn.get", json!({"id": rows[0]["id"]}));
    assert_eq!(full["external_ref"], "F1");
    assert_eq!(
        h.ok("import.ofx", json!({"account": "Bank", "file": f}))["duplicates_skipped"],
        2
    );
}

#[test]
fn ofx_rejects_wrong_currency_and_garbage() {
    let h = H::new();
    h.ok(
        "acct.add",
        json!({"name": "Usd", "kind": "checking", "currency": "USD"}),
    );
    let f = file(&h, "s.ofx", OFX);
    assert_eq!(
        h.err("import.ofx", json!({"account": "Usd", "file": f}))
            .code,
        "currency_mismatch"
    );
    h.acct("Bank", "checking", "0");
    let g = file(&h, "g.ofx", "hello world");
    assert_eq!(
        h.err("import.ofx", json!({"account": "Bank", "file": g}))
            .code,
        "invalid_ofx"
    );
}

#[test]
fn imported_payees_are_normalised_and_categories_applied() {
    let h = H::new();
    h.acct("Bank", "checking", "0");
    h.ok(
        "payee.alias.add",
        json!({"alias": "upi-swiggy-123", "canonical": "Swiggy"}),
    );
    let f = file(
        &h,
        "c.csv",
        "date,desc,amount,cat\n2026-10-01,UPI-SWIGGY-123,-100,food.delivery\n2026-10-02,Misc,-5,\n",
    );
    h.ok("import.csv", json!({"account": "Bank", "file": f, "date_col": "date", "payee_col": "desc", "amount_col": "amount", "category_col": "cat"}));
    let rows = h.ok("txn.list", json!({"sort": "date"}));
    assert_eq!(rows[0]["payee"], "Swiggy");
    assert_eq!(rows[0]["category"], "food.delivery");
    assert!(rows[1]["category"].is_null());
    // an unknown category aborts the whole import
    let f2 = file(&h, "c2.csv", "date,desc,amount,cat\n2026-10-03,X,-1,nope\n");
    let e = h.err("import.csv", json!({"account": "Bank", "file": f2, "date_col": "date", "payee_col": "desc", "amount_col": "amount", "category_col": "cat"}));
    assert_eq!(e.code, "import_errors");
}
