use super::*;

fn script(src: &str) -> Script {
    match parse_script(src) {
        Ok(s) => s,
        Err(e) => panic!("`{src}` failed at line {}: {}", e.line, e.message),
    }
}

/// The argument of a call is an **expression**, however it begins: `age_plus_n(my_age(), 3)`
/// once failed with "expected `,` or `)`".
#[test]
fn a_call_can_be_an_argument() {
    for src in [
        "def my_age() { 41 }\ndef age_plus_n(a, n) { a + n }\nlet x = age_plus_n(my_age(), 3)",
        "def add(a, b) { a + b }\nformat(\"add={}\", add(2, 3))",
        "format(\"n={}\", count([1, 2, 3]))",
        // A qualified name — §9h's `use … as` — which failed for a *different* reason.
        "format(\"x={}\", utils.helper(1))",
        "def f(a) { a }\nf(f(f(1)))",
        "def g(a, b) { a }\ndef h() { 1 }\ng(h(), b: h())",
        // The workaround this replaces, which must keep working.
        "def add(a, b) { a + b }\nlet sum = add(2, 3)\nformat(\"add={}\", sum)",
        "format(\"len={}\", (\"hello\" | count))",
    ] {
        script(src);
    }
}

/// An argument starting with an identifier binds like any other: `format("{}", a + b * c)` once
/// folded flat into `(a + b) * c` through a hand-rolled copy of the tiers.
#[test]
fn an_argument_starting_with_a_name_binds_like_any_other() {
    let by_name = one_arg("format(\"{}\", a + b * c)");
    let by_literal = one_arg("format(\"{}\", 1 + 2 * 3)");
    assert!(
        matches!(by_name, Expr::Binary(BinOp::Add, _, _)),
        "`a + b * c` as an argument parsed as {by_name:?}, not an addition"
    );
    assert!(matches!(by_literal, Expr::Binary(BinOp::Add, _, _)), "the literal control changed shape: {by_literal:?}");
}

/// Every tier below the top one, exercised from argument position.
#[test]
fn an_argument_reaches_every_tier() {
    for (src, want) in [
        ("format(\"{}\", a ?? b)", BinOp::Coalesce),
        ("format(\"{}\", a || b && c)", BinOp::Or),
        ("format(\"{}\", a == b)", BinOp::Eq),
        ("format(\"{}\", a ++ b)", BinOp::Concat),
        ("format(\"{}\", a - b)", BinOp::Sub),
    ] {
        match one_arg(src) {
            Expr::Binary(op, _, _) => assert_eq!(op, want, "{src}"),
            other => panic!("{src} parsed as {other:?}"),
        }
    }
    // Postfix still binds tighter than any of them.
    assert!(matches!(one_arg("format(\"{}\", a.b + c)"), Expr::Binary(BinOp::Add, _, _)));
    assert!(matches!(one_arg("format(\"{}\", a[0] + c)"), Expr::Binary(BinOp::Add, _, _)));
}

/// The second positional argument of the one call in `src`.
fn one_arg(src: &str) -> Expr {
    let s = script(src);
    let Stmt::Expr(Expr::Call(c)) = &s.stmts[0] else { panic!("{src}: not a call") };
    match &c.args[1] {
        Arg::Positional(e) => e.clone(),
        other => panic!("{src}: argument 1 is {other:?}"),
    }
}
